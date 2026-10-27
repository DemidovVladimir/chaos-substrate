//! Project-wide analysis over per-file extractions: builds the L0 graph
//! (files, symbols, surfaces, containment) and resolves imports, calls and
//! by-name relations across files. Pure and deterministic — no I/O, no
//! embedder; the output feeds `application::chunk`.
//!
//! Resolution policy (never fuzzy):
//! * imports → `Imports` edges between File nodes (confidence 1.0); bare
//!   package specifiers are recorded in the File node metadata `packages`.
//! * calls → `Calls` edges only when unambiguous:
//!   (a) exactly one definition of the callee in the same file → 1.0;
//!   (b) exactly one among the files this file imports → 0.9;
//!   (c) receiver-qualified: `self`/`this` → the caller's container's method,
//!   or receiver == a container/type name → its method (exactly one) → 0.8;
//!   (d) exactly one definition repo-wide (unqualified calls only) → 0.7;
//!   otherwise dropped.
//! * relations → `Extends`/`Implements` (same file → imports → repo-unique),
//!   `Queries` (SDL `Field` by qualified name), `Defines` (same file).

use crate::domain::hash;
use crate::domain::model::{Edge, EdgeKind, IndexBundle, Language, Node, NodeKind, SourceFile};
use crate::ports::extractor::{FileExtraction, FileInput, SurfaceKind, Symbol};
use crate::ports::index_store::Graph;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Counters reported by [`analyze`]; compact enough to go into a tool return.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct AnalyzeStats {
    pub files: usize,
    pub symbols: usize,
    pub imports_resolved: usize,
    pub imports_total: usize,
    pub calls_resolved: usize,
    pub calls_total: usize,
    pub relations_resolved: usize,
}

/// Everything chunking needs for one code-bearing symbol.
#[derive(Debug, Clone)]
pub struct Unit {
    /// Node id of the symbol (`hash::scoped_id(repo_id, stable_id)`).
    pub node_id: Uuid,
    /// Index into `Analyzed.bundle.files` / the input slice.
    pub file_index: usize,
    /// The extracted symbol (byte range, segments, doc, lines, kind, name).
    pub symbol: Symbol,
    /// Qualified name of the enclosing container, if any.
    pub container: Option<String>,
    /// Imported names (from resolved imports) that appear in the body.
    pub uses: Vec<String>,
    /// Resolved callee names of calls made from this symbol.
    pub calls: Vec<String>,
}

/// Per-file leftovers chunking needs for the `file_head` chunk.
#[derive(Debug, Clone, Default)]
pub struct FileHead {
    pub file_index: usize,
    /// Byte ranges not owned by any symbol, in source order.
    pub free_ranges: Vec<(usize, usize)>,
    /// Import specifiers as written, in source order.
    pub imports: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Analyzed {
    /// files, nodes, edges — NO chunks yet.
    pub bundle: IndexBundle,
    pub stats: AnalyzeStats,
    pub units: Vec<Unit>,
    /// One entry per input file, same order.
    pub heads: Vec<FileHead>,
}

const CALL_HINT_CAP: usize = 12;

const CONF_SAME_FILE: f32 = 1.0;
const CONF_IMPORTED: f32 = 0.9;
const CONF_RECEIVER: f32 = 0.8;
const CONF_REPO_UNIQUE: f32 = 0.7;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct SymRef {
    file: usize,
    sym: usize,
}

/// The slice of a symbol resolution looks at — the same shape whether the
/// symbol comes from a changed file's extraction or from the persisted graph.
#[derive(Debug, Clone)]
struct SymView<'a> {
    stable_id: &'a str,
    kind: NodeKind,
    name: &'a str,
    qualified_name: &'a str,
    parent: Option<&'a str>,
}

/// One file in the resolution universe: the changed files come first (same
/// order as the input slice), the known (persisted, unchanged) files after.
#[derive(Debug, Clone)]
struct FileView<'a> {
    path: &'a str,
    language: Language,
    symbols: Vec<SymView<'a>>,
}

/// Build the L0 graph and resolve cross-file references over `files` alone
/// (the whole repository). Production callers go through
/// [`analyze_with`]; this stays as the closed-world entry point for tests.
#[cfg_attr(not(test), allow(dead_code))]
pub fn analyze(repo_id: Uuid, files: &[(FileInput, FileExtraction)]) -> Analyzed {
    analyze_with(repo_id, files, None)
}

/// Build the L0 graph for `files` and resolve their imports, calls and
/// relations against `files` PLUS the persisted graph `known` (when given).
///
/// Known symbols (every non-file, non-surface node, located through its file's
/// path and qualified via `metadata.qualified_name`) and known File nodes take
/// part in the definition index and import resolution exactly as if their files
/// had been analyzed alongside — so `Unit.uses`/`Unit.calls` (the chunk header)
/// and the resolved edge targets are identical to a full analyze of the whole
/// repository. Known nodes whose file is in `files` are dropped first (the file
/// is being re-extracted). The returned bundle holds ONLY `files`' nodes, edges
/// and units; known nodes are referenced by id in edges, never re-emitted.
pub fn analyze_with(
    repo_id: Uuid,
    files: &[(FileInput, FileExtraction)],
    known: Option<&Graph>,
) -> Analyzed {
    let mut out = Analyzed::default();
    let mut edges = EdgeSet::default();
    let node_id_of = |stable_id: &str| hash::scoped_id(repo_id, stable_id);

    // ---- resolution universe ------------------------------------------------
    let n = files.len();
    let mut view: Vec<FileView<'_>> = files
        .iter()
        .map(|(input, x)| FileView {
            path: input.path.as_str(),
            language: x.language.unwrap_or(input.language),
            symbols: x
                .symbols
                .iter()
                .map(|s| SymView {
                    stable_id: s.stable_id.as_str(),
                    kind: s.kind,
                    name: s.name.as_str(),
                    qualified_name: s.qualified_name.as_str(),
                    parent: s.parent.as_deref(),
                })
                .collect(),
        })
        .collect();
    if let Some(g) = known {
        view.extend(known_views(g, files));
    }

    // ---- lookup tables -----------------------------------------------------
    let path_index: HashMap<&str, usize> =
        view.iter().enumerate().map(|(i, f)| (f.path, i)).collect();
    let mut sym_by_id: HashMap<&str, SymRef> = HashMap::new();
    let mut defs_by_name: HashMap<&str, Vec<SymRef>> = HashMap::new();
    let mut typeish_by_name: HashMap<&str, Vec<SymRef>> = HashMap::new();
    let mut fields_by_qualified: HashMap<&str, Vec<SymRef>> = HashMap::new();
    for (fi, f) in view.iter().enumerate() {
        for (si, s) in f.symbols.iter().enumerate() {
            let r = SymRef { file: fi, sym: si };
            sym_by_id.insert(s.stable_id, r);
            if is_callable(s.kind) {
                defs_by_name.entry(s.name).or_default().push(r);
            }
            if is_typeish(s.kind) {
                typeish_by_name.entry(s.name).or_default().push(r);
            }
            if s.kind == NodeKind::Field {
                fields_by_qualified
                    .entry(s.qualified_name)
                    .or_default()
                    .push(r);
            }
        }
    }

    // ---- repository + file nodes -------------------------------------------
    let repo_node = hash::scoped_id(repo_id, "repo");
    out.bundle.nodes.push(Node {
        id: repo_node,
        repo_id,
        file_id: None,
        kind: NodeKind::Repository,
        stable_id: "repo".to_string(),
        name: "repo".to_string(),
        signature: None,
        doc: None,
        line_start: None,
        line_end: None,
        metadata: json!({}),
    });

    // File node ids for the whole universe (known File nodes share the same
    // uuid5 derivation, so an edge into one lands on the persisted node).
    let file_nodes: Vec<Uuid> = view
        .iter()
        .map(|f| node_id_of(&file_stable_id(f.path)))
        .collect();
    let mut file_ids: Vec<Uuid> = Vec::with_capacity(n);
    for (input, x) in files {
        let language = x.language.unwrap_or(input.language);
        let file_id = hash::scoped_id(repo_id, &format!("source_file:{}", input.path));
        let node_id = node_id_of(&file_stable_id(&input.path));
        file_ids.push(file_id);
        out.bundle.files.push(SourceFile {
            id: file_id,
            repo_id,
            path: input.path.clone(),
            language,
            content_hash: x.content_hash.clone(),
            line_count: x.line_count as i32,
        });
        out.bundle.nodes.push(Node {
            id: node_id,
            repo_id,
            file_id: Some(file_id),
            kind: NodeKind::File,
            stable_id: file_stable_id(&input.path),
            name: input.path.clone(),
            signature: None,
            doc: None,
            line_start: Some(1),
            line_end: Some(x.line_count.max(1) as i32),
            metadata: json!({
                "language": language.as_str(),
                "packages": x.packages,
                "line_count": x.line_count,
            }),
        });
        edges.push(repo_id, repo_node, node_id, EdgeKind::Contains, 1.0);
    }

    // ---- symbol nodes + containment ----------------------------------------
    for (fi, (input, x)) in files.iter().enumerate() {
        let language = x.language.unwrap_or(input.language);
        for s in &x.symbols {
            let id = node_id_of(&s.stable_id);
            let mut metadata = match &s.metadata {
                Value::Object(m) => Value::Object(m.clone()),
                _ => json!({}),
            };
            if let Value::Object(m) = &mut metadata {
                m.insert("qualified_name".into(), json!(s.qualified_name));
                m.insert("language".into(), json!(language.as_str()));
                m.insert("path".into(), json!(input.path));
            }
            out.bundle.nodes.push(Node {
                id,
                repo_id,
                file_id: Some(file_ids[fi]),
                kind: s.kind,
                stable_id: s.stable_id.clone(),
                name: s.name.clone(),
                signature: s.signature.clone(),
                doc: s.doc.clone(),
                line_start: Some(s.line_start as i32),
                line_end: Some(s.line_end as i32),
                metadata,
            });
            let parent = s
                .parent
                .as_deref()
                .filter(|p| sym_by_id.get(p).is_some_and(|r| r.file == fi))
                .map(node_id_of)
                .unwrap_or(file_nodes[fi]);
            edges.push(repo_id, parent, id, EdgeKind::Contains, 1.0);
        }
        out.stats.symbols += x.symbols.len();

        // Surfaces: one node per (file, kind, name); defined by the file and by
        // every owning symbol.
        let mut seen: HashSet<String> = HashSet::new();
        for surf in &x.surfaces {
            let (tag, kind) = match surf.kind {
                SurfaceKind::EnvVar => ("env", NodeKind::EnvVar),
                SurfaceKind::HttpRoute => ("route", NodeKind::HttpRoute),
            };
            let stable_id = format!("{}:{}:{}", input.path, tag, surf.name);
            let id = node_id_of(&stable_id);
            if seen.insert(stable_id.clone()) {
                out.bundle.nodes.push(Node {
                    id,
                    repo_id,
                    file_id: Some(file_ids[fi]),
                    kind,
                    stable_id,
                    name: surf.name.clone(),
                    signature: None,
                    doc: None,
                    line_start: Some(surf.line as i32),
                    line_end: Some(surf.line as i32),
                    metadata: json!({"path": input.path, "language": language.as_str()}),
                });
                edges.push(repo_id, file_nodes[fi], id, EdgeKind::Defines, 1.0);
            }
            if let Some(owner) = surf.owner.as_deref() {
                if sym_by_id.get(owner).is_some_and(|r| r.file == fi) {
                    edges.push(repo_id, node_id_of(owner), id, EdgeKind::Defines, 1.0);
                }
            }
        }
    }
    out.stats.files = n;

    // ---- imports ------------------------------------------------------------
    // imported_files[fi] = files fi imports (resolved, may be known), first-seen order.
    let mut imported_files: Vec<Vec<usize>> = vec![Vec::new(); n];
    // imported_names[fi] = names available from resolved imports (uses hints).
    let mut imported_names: Vec<Vec<String>> = vec![Vec::new(); n];
    for (fi, (input, x)) in files.iter().enumerate() {
        let language = view[fi].language;
        out.heads.push(FileHead {
            file_index: fi,
            free_ranges: x.free_ranges.clone(),
            imports: x.imports.iter().map(|i| i.specifier.clone()).collect(),
        });
        for imp in &x.imports {
            out.stats.imports_total += 1;
            let targets = resolve_import(
                language,
                &imp.specifier,
                &imp.names,
                &input.path,
                &path_index,
            );
            let targets: Vec<usize> = targets.into_iter().filter(|t| *t != fi).collect();
            if targets.is_empty() {
                continue;
            }
            out.stats.imports_resolved += 1;
            for t in targets {
                if !imported_files[fi].contains(&t) {
                    imported_files[fi].push(t);
                }
                edges.push(
                    repo_id,
                    file_nodes[fi],
                    file_nodes[t],
                    EdgeKind::Imports,
                    1.0,
                );
            }
            if imp.names.is_empty() {
                if let Some(last) = last_segment(&imp.specifier) {
                    push_unique(&mut imported_names[fi], last.to_string());
                }
            } else {
                for n in &imp.names {
                    push_unique(&mut imported_names[fi], n.clone());
                }
            }
        }
    }

    // ---- calls --------------------------------------------------------------
    // resolved callee names per caller stable_id, in source order.
    let mut calls_by_caller: HashMap<&str, Vec<String>> = HashMap::new();
    for (fi, (_, x)) in files.iter().enumerate() {
        for call in &x.calls {
            out.stats.calls_total += 1;
            let Some((target, conf)) = resolve_call(
                fi,
                call.caller.as_str(),
                call.callee.as_str(),
                call.receiver.as_deref(),
                &view,
                &sym_by_id,
                &defs_by_name,
                &imported_files,
            ) else {
                continue;
            };
            let target_sym = &view[target.file].symbols[target.sym];
            let source = if call.caller.starts_with("file:") {
                file_nodes[fi]
            } else if sym_by_id.contains_key(call.caller.as_str()) {
                node_id_of(&call.caller)
            } else {
                file_nodes[fi]
            };
            let target_id = node_id_of(target_sym.stable_id);
            if source == target_id {
                continue;
            }
            out.stats.calls_resolved += 1;
            edges.push(repo_id, source, target_id, EdgeKind::Calls, conf);
            push_unique(
                calls_by_caller.entry(call.caller.as_str()).or_default(),
                target_sym.name.to_string(),
            );
        }
    }

    // ---- relations ----------------------------------------------------------
    for (fi, (_, x)) in files.iter().enumerate() {
        for rel in &x.relations {
            let source = if rel.source.starts_with("file:") {
                file_nodes[fi]
            } else if sym_by_id.contains_key(rel.source.as_str()) {
                node_id_of(&rel.source)
            } else {
                continue;
            };
            let resolved: Option<(SymRef, f32)> = match rel.kind {
                EdgeKind::Extends | EdgeKind::Implements => {
                    let name = last_segment(&rel.target_name).unwrap_or(&rel.target_name);
                    let cands = typeish_by_name.get(name).map(Vec::as_slice).unwrap_or(&[]);
                    pick_scoped(cands, fi, &imported_files[fi])
                }
                EdgeKind::Queries => fields_by_qualified
                    .get(rel.target_name.as_str())
                    .and_then(|c| unique(c))
                    .map(|r| (r, CONF_SAME_FILE)),
                EdgeKind::Defines => {
                    let cands: Vec<SymRef> = x
                        .symbols
                        .iter()
                        .enumerate()
                        .filter(|(_, s)| {
                            matches!(s.kind, NodeKind::Operation | NodeKind::Fragment)
                                && (s.qualified_name == rel.target_name
                                    || s.name == rel.target_name)
                        })
                        .map(|(si, _)| SymRef { file: fi, sym: si })
                        .collect();
                    unique(&cands).map(|r| (r, CONF_SAME_FILE))
                }
                _ => None,
            };
            let Some((target, conf)) = resolved else {
                continue;
            };
            let target_id = node_id_of(view[target.file].symbols[target.sym].stable_id);
            if target_id == source {
                continue;
            }
            out.stats.relations_resolved += 1;
            edges.push(repo_id, source, target_id, rel.kind, conf);
        }
    }

    // ---- units --------------------------------------------------------------
    for (fi, (input, x)) in files.iter().enumerate() {
        for s in &x.symbols {
            if !s.kind.is_unit() {
                continue;
            }
            let body = slice_body(&input.content, s.byte_start, s.byte_end);
            let mut uses: Vec<String> = imported_names[fi]
                .iter()
                .filter(|n| contains_identifier(body, n))
                .cloned()
                .collect();
            uses.sort();
            uses.dedup();
            uses.truncate(CALL_HINT_CAP);
            let mut calls = calls_by_caller
                .get(s.stable_id.as_str())
                .cloned()
                .unwrap_or_default();
            calls.sort();
            calls.dedup();
            calls.truncate(CALL_HINT_CAP);
            let container = s
                .parent
                .as_deref()
                .and_then(|p| sym_by_id.get(p))
                .map(|r| view[r.file].symbols[r.sym].qualified_name.to_string());
            out.units.push(Unit {
                node_id: node_id_of(&s.stable_id),
                file_index: fi,
                symbol: s.clone(),
                container,
                uses,
                calls,
            });
        }
    }

    out.bundle.edges = edges.into_vec();
    tracing::info!(
        files = out.stats.files,
        known_files = view.len() - n,
        symbols = out.stats.symbols,
        nodes = out.bundle.nodes.len(),
        edges = out.bundle.edges.len(),
        imports = format!("{}/{}", out.stats.imports_resolved, out.stats.imports_total),
        calls = format!("{}/{}", out.stats.calls_resolved, out.stats.calls_total),
        relations = out.stats.relations_resolved,
        "analyzed"
    );
    out
}

/// Project the persisted graph onto [`FileView`]s: one per known file that is
/// NOT in the changed set, carrying its symbol nodes (everything but the
/// repository/file/surface nodes) with parents recovered from `Contains`
/// edges. Ordered by path, symbols by stable_id — deterministic regardless of
/// the store's row order.
fn known_views<'a>(g: &'a Graph, changed: &[(FileInput, FileExtraction)]) -> Vec<FileView<'a>> {
    let changed_paths: HashSet<&str> = changed.iter().map(|(f, _)| f.path.as_str()).collect();
    let changed_ids: HashSet<&str> = changed
        .iter()
        .flat_map(|(_, x)| x.symbols.iter().map(|s| s.stable_id.as_str()))
        .collect();
    let node_by_id: HashMap<Uuid, &Node> = g.nodes.iter().map(|n| (n.id, n)).collect();
    // child id → parent stable_id, when the parent is a symbol (not a file).
    let parent_of: HashMap<Uuid, &str> = g
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Contains)
        .filter_map(|e| {
            let p = node_by_id.get(&e.source_node_id)?;
            (is_symbol_kind(p.kind)).then_some((e.target_node_id, p.stable_id.as_str()))
        })
        .collect();
    let mut file_pos: HashMap<Uuid, usize> = HashMap::new();
    let mut views: Vec<FileView<'a>> = Vec::new();
    let mut files: Vec<&SourceFile> = g
        .files
        .iter()
        .filter(|f| !changed_paths.contains(f.path.as_str()))
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files.dedup_by(|a, b| a.path == b.path);
    for f in files {
        file_pos.insert(f.id, views.len());
        views.push(FileView {
            path: f.path.as_str(),
            language: f.language,
            symbols: Vec::new(),
        });
    }
    let mut nodes: Vec<&Node> = g
        .nodes
        .iter()
        .filter(|nd| is_symbol_kind(nd.kind) && !changed_ids.contains(nd.stable_id.as_str()))
        .collect();
    nodes.sort_by(|a, b| a.stable_id.cmp(&b.stable_id));
    for nd in nodes {
        let Some(&pos) = nd.file_id.and_then(|id| file_pos.get(&id)) else {
            continue;
        };
        let qualified_name = nd
            .metadata
            .get("qualified_name")
            .and_then(Value::as_str)
            .unwrap_or(nd.name.as_str());
        views[pos].symbols.push(SymView {
            stable_id: nd.stable_id.as_str(),
            kind: nd.kind,
            name: nd.name.as_str(),
            qualified_name,
            parent: parent_of.get(&nd.id).copied(),
        });
    }
    views
}

fn is_symbol_kind(kind: NodeKind) -> bool {
    !matches!(
        kind,
        NodeKind::Repository | NodeKind::File | NodeKind::EnvVar | NodeKind::HttpRoute
    )
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Insertion-ordered, id-deduplicated edge collector.
#[derive(Default)]
struct EdgeSet {
    seen: HashSet<Uuid>,
    edges: Vec<Edge>,
}

impl EdgeSet {
    fn push(&mut self, repo_id: Uuid, source: Uuid, target: Uuid, kind: EdgeKind, confidence: f32) {
        if source == target {
            return;
        }
        let id = hash::edge_id(source, target, kind.as_str());
        if self.seen.insert(id) {
            self.edges.push(Edge {
                id,
                repo_id,
                source_node_id: source,
                target_node_id: target,
                kind,
                confidence,
            });
        }
    }
    fn into_vec(self) -> Vec<Edge> {
        self.edges
    }
}

pub fn file_stable_id(path: &str) -> String {
    format!("file:{path}")
}

fn is_callable(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Function | NodeKind::Method | NodeKind::Type | NodeKind::Interface
    )
}

fn is_typeish(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Type | NodeKind::Interface | NodeKind::Enum)
}

fn push_unique(v: &mut Vec<String>, s: String) {
    if !v.contains(&s) {
        v.push(s);
    }
}

fn unique(c: &[SymRef]) -> Option<SymRef> {
    match c {
        [one] => Some(*one),
        _ => None,
    }
}

/// Same file (1.0) → imported files (0.9) → repo-unique (0.7).
fn pick_scoped(cands: &[SymRef], fi: usize, imported: &[usize]) -> Option<(SymRef, f32)> {
    let same: Vec<SymRef> = cands.iter().copied().filter(|r| r.file == fi).collect();
    if let Some(r) = unique(&same) {
        return Some((r, CONF_SAME_FILE));
    }
    let imp: Vec<SymRef> = cands
        .iter()
        .copied()
        .filter(|r| imported.contains(&r.file))
        .collect();
    if let Some(r) = unique(&imp) {
        return Some((r, CONF_IMPORTED));
    }
    unique(cands).map(|r| (r, CONF_REPO_UNIQUE))
}

fn last_segment(spec: &str) -> Option<&str> {
    spec.rsplit(['/', '.', ':']).find(|s| !s.is_empty())
}

fn slice_body(content: &str, start: usize, end: usize) -> &str {
    let len = content.len();
    let mut s = start.min(len);
    let mut e = end.min(len).max(s);
    while s > 0 && !content.is_char_boundary(s) {
        s -= 1;
    }
    while e < len && !content.is_char_boundary(e) {
        e += 1;
    }
    &content[s..e]
}

fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Whole-identifier occurrence check (ASCII word boundaries).
fn contains_identifier(body: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let b = body.as_bytes();
    let mut from = 0;
    while let Some(pos) = body[from..].find(name) {
        let start = from + pos;
        let end = start + name.len();
        let before = start == 0 || !is_ident_char(b[start - 1]);
        let after = end >= b.len() || !is_ident_char(b[end]);
        if before && after {
            return true;
        }
        from = start + 1;
        if from >= body.len() {
            break;
        }
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn resolve_call(
    fi: usize,
    caller: &str,
    callee: &str,
    receiver: Option<&str>,
    files: &[FileView<'_>],
    sym_by_id: &HashMap<&str, SymRef>,
    defs_by_name: &HashMap<&str, Vec<SymRef>>,
    imported_files: &[Vec<usize>],
) -> Option<(SymRef, f32)> {
    let defs = defs_by_name.get(callee).map(Vec::as_slice).unwrap_or(&[]);
    let receiver = receiver.map(str::trim).filter(|r| !r.is_empty());
    let self_like = matches!(receiver, Some("self" | "this" | "Self"));

    // (c) receiver-qualified takes precedence when the receiver names a container:
    // it is strictly more specific than a same-file name match.
    if self_like {
        if let Some(container) = container_of(caller, sym_by_id, files) {
            let cont_name = files[container.file].symbols[container.sym].name;
            let cands: Vec<SymRef> = defs
                .iter()
                .copied()
                .filter(|r| r.file == container.file)
                .filter(|r| parent_name(*r, files, sym_by_id) == Some(cont_name))
                .collect();
            if let Some(r) = unique(&cands) {
                return Some((r, CONF_RECEIVER));
            }
        }
    } else if let Some(recv) = receiver {
        let cands: Vec<SymRef> = defs
            .iter()
            .copied()
            .filter(|r| files[r.file].symbols[r.sym].kind == NodeKind::Method)
            .filter(|r| parent_name(*r, files, sym_by_id) == Some(recv))
            .collect();
        if let Some(r) = unique(&cands) {
            return Some((r, CONF_RECEIVER));
        }
        // A type-qualified call (`Vec::new`, `Math.max`) whose type is not a
        // container we know is foreign: never bind it to a local name.
        if recv.starts_with(|c: char| c.is_ascii_uppercase()) {
            return None;
        }
    }

    // (a) same file
    let same: Vec<SymRef> = defs.iter().copied().filter(|r| r.file == fi).collect();
    if let Some(r) = unique(&same) {
        return Some((r, CONF_SAME_FILE));
    }
    // (b) imported files
    let imp: Vec<SymRef> = defs
        .iter()
        .copied()
        .filter(|r| imported_files[fi].contains(&r.file))
        .collect();
    if let Some(r) = unique(&imp) {
        return Some((r, CONF_IMPORTED));
    }
    // (d) repo-unique, unqualified calls only (a foreign receiver like `Vec::new`
    // must not bind to the one local `new`).
    if receiver.is_none() || self_like {
        if let Some(r) = unique(defs) {
            return Some((r, CONF_REPO_UNIQUE));
        }
    }
    None
}

/// The container a `self`/`this` call inside `caller` refers to: the caller's
/// parent when it is a member, or the caller itself when it is a container.
fn container_of(
    caller: &str,
    sym_by_id: &HashMap<&str, SymRef>,
    files: &[FileView<'_>],
) -> Option<SymRef> {
    let r = *sym_by_id.get(caller)?;
    let s = &files[r.file].symbols[r.sym];
    match s.kind {
        NodeKind::Type | NodeKind::Interface | NodeKind::Module | NodeKind::Enum => Some(r),
        _ => s.parent.and_then(|p| sym_by_id.get(p)).copied(),
    }
}

fn parent_name<'a>(
    r: SymRef,
    files: &'a [FileView<'a>],
    sym_by_id: &HashMap<&str, SymRef>,
) -> Option<&'a str> {
    let s = &files[r.file].symbols[r.sym];
    let p = *sym_by_id.get(s.parent?)?;
    Some(files[p.file].symbols[p.sym].name)
}

// ---- path utilities ---------------------------------------------------------

fn dir_of(path: &str) -> &str {
    path.rfind('/').map(|i| &path[..i]).unwrap_or("")
}

fn parent_dir(dir: &str) -> &str {
    dir.rfind('/').map(|i| &dir[..i]).unwrap_or("")
}

fn join(base: &str, rel: &str) -> String {
    if base.is_empty() {
        rel.to_string()
    } else if rel.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{rel}")
    }
}

/// Join `rel` onto `base` and collapse `.` / `..` components.
fn normalize(base: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rfind('.').map(|i| &name[..i]).unwrap_or(name)
}

fn resolve_import(
    language: Language,
    specifier: &str,
    names: &[String],
    cur_path: &str,
    paths: &HashMap<&str, usize>,
) -> Vec<usize> {
    match language {
        Language::Rust => resolve_rust(specifier, names, cur_path, paths),
        Language::TypeScript | Language::JavaScript => {
            resolve_ts(specifier, cur_path, paths).into_iter().collect()
        }
        Language::Python => resolve_python(specifier, names, cur_path, paths),
        Language::Solidity => resolve_solidity(specifier, cur_path, paths)
            .into_iter()
            .collect(),
        Language::GraphQL | Language::Markdown => Vec::new(),
    }
}

/// Directory whose files make up the module `cur_path` declares items in:
/// `src/a/b.rs` → `src/a/b`; `src/a/mod.rs`, `src/lib.rs`, `src/main.rs` → dir.
fn rust_module_dir(cur_path: &str) -> String {
    let dir = dir_of(cur_path);
    match stem(cur_path) {
        "mod" | "lib" | "main" => dir.to_string(),
        s => join(dir, s),
    }
}

/// Nearest `src` directory on the path, else the repository root.
fn rust_crate_root(cur_path: &str) -> String {
    let parts: Vec<&str> = cur_path.split('/').collect();
    parts[..parts.len().saturating_sub(1)]
        .iter()
        .rposition(|p| *p == "src")
        .map(|i| parts[..=i].join("/"))
        .unwrap_or_default()
}

fn rust_probe(base: &str, segs: &[&str], paths: &HashMap<&str, usize>) -> Option<usize> {
    let p = join(base, &segs.join("/"));
    if segs.is_empty() {
        return [
            format!("{p}.rs"),
            join(&p, "mod.rs"),
            join(&p, "lib.rs"),
            join(&p, "main.rs"),
        ]
        .iter()
        .find_map(|c| paths.get(c.as_str()).copied());
    }
    [format!("{p}.rs"), join(&p, "mod.rs")]
        .iter()
        .find_map(|c| paths.get(c.as_str()).copied())
}

fn resolve_rust(
    spec: &str,
    names: &[String],
    cur_path: &str,
    paths: &HashMap<&str, usize>,
) -> Vec<usize> {
    let segs: Vec<&str> = spec
        .split("::")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let Some(first) = segs.first() else {
        return Vec::new();
    };
    let (mut base, mut rest): (String, &[&str]) = match *first {
        "crate" => (rust_crate_root(cur_path), &segs[1..]),
        "self" => (rust_module_dir(cur_path), &segs[1..]),
        "super" => (rust_module_dir(cur_path), &segs[..]),
        _ => return Vec::new(), // bare package (recorded in `packages`)
    };
    while rest.first() == Some(&"super") {
        base = parent_dir(&base).to_string();
        rest = &rest[1..];
    }
    let mut out = Vec::new();
    // Longest prefix first: crate::a::b::Foo → a/b.rs before a.rs.
    for k in (0..=rest.len()).rev() {
        if let Some(hit) = rust_probe(&base, &rest[..k], paths) {
            out.push(hit);
            break;
        }
    }
    // `use crate::a::{b, c}` — the names may be modules themselves.
    for n in names {
        let mut segs: Vec<&str> = rest.to_vec();
        segs.push(n.as_str());
        if let Some(hit) = rust_probe(&base, &segs, paths) {
            if !out.contains(&hit) {
                out.push(hit);
            }
        }
    }
    out
}

const TS_EXTS: [&str; 7] = [".ts", ".tsx", ".js", ".jsx", ".mts", ".cts", ".mjs"];

fn resolve_ts(spec: &str, cur_path: &str, paths: &HashMap<&str, usize>) -> Option<usize> {
    if !spec.starts_with('.') {
        return None;
    }
    let target = normalize(dir_of(cur_path), spec);
    if let Some(&i) = paths.get(target.as_str()) {
        return Some(i);
    }
    // ESM-style `./x.js` written for a `.ts` source.
    let bare = [".js", ".jsx", ".mjs", ".cjs"]
        .iter()
        .find_map(|e| target.strip_suffix(e))
        .unwrap_or(target.as_str());
    for ext in TS_EXTS {
        if let Some(&i) = paths.get(format!("{bare}{ext}").as_str()) {
            return Some(i);
        }
    }
    for ext in TS_EXTS {
        if let Some(&i) = paths.get(format!("{target}/index{ext}").as_str()) {
            return Some(i);
        }
    }
    None
}

fn py_probe(base: &str, segs: &[&str], paths: &HashMap<&str, usize>) -> Option<usize> {
    let p = join(base, &segs.join("/"));
    if segs.is_empty() {
        return paths.get(join(&p, "__init__.py").as_str()).copied();
    }
    [format!("{p}.py"), join(&p, "__init__.py")]
        .iter()
        .find_map(|c| paths.get(c.as_str()).copied())
}

fn resolve_python(
    spec: &str,
    names: &[String],
    cur_path: &str,
    paths: &HashMap<&str, usize>,
) -> Vec<usize> {
    let dots = spec.bytes().take_while(|b| *b == b'.').count();
    let rest: Vec<&str> = spec[dots..].split('.').filter(|s| !s.is_empty()).collect();
    let roots: Vec<String> = if dots > 0 {
        let mut base = dir_of(cur_path).to_string();
        for _ in 1..dots {
            base = parent_dir(&base).to_string();
        }
        vec![base]
    } else {
        // Repository root first, then the file's directory ancestors (src layouts).
        let mut roots = vec![String::new()];
        let mut d = dir_of(cur_path);
        while !d.is_empty() {
            roots.push(d.to_string());
            d = parent_dir(d);
        }
        roots
    };
    for root in &roots {
        let mut out = Vec::new();
        for k in (0..=rest.len()).rev() {
            if dots == 0 && k == 0 {
                break;
            }
            if let Some(hit) = py_probe(root, &rest[..k], paths) {
                out.push(hit);
                break;
            }
        }
        for n in names {
            let mut segs = rest.clone();
            segs.push(n.as_str());
            if let Some(hit) = py_probe(root, &segs, paths) {
                if !out.contains(&hit) {
                    out.push(hit);
                }
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    Vec::new()
}

fn resolve_solidity(spec: &str, cur_path: &str, paths: &HashMap<&str, usize>) -> Option<usize> {
    if !spec.starts_with('.') {
        return None;
    }
    paths
        .get(normalize(dir_of(cur_path), spec).as_str())
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::extractor::{CallCandidate, ImportRef, Relation, Surface};

    fn repo() -> Uuid {
        hash::repo_id("/test/repo")
    }

    fn sym(stable_id: &str, kind: NodeKind, name: &str, parent: Option<&str>) -> Symbol {
        let qualified = stable_id.rsplit(':').next().unwrap().to_string();
        Symbol {
            stable_id: stable_id.to_string(),
            kind,
            name: name.to_string(),
            qualified_name: qualified,
            parent: parent.map(str::to_string),
            signature: None,
            doc: None,
            line_start: 1,
            line_end: 2,
            byte_start: 0,
            byte_end: 0,
            segments: vec![],
            metadata: json!({}),
        }
    }

    fn file(path: &str, content: &str, symbols: Vec<Symbol>) -> (FileInput, FileExtraction) {
        let language = Language::from_path(path).unwrap();
        (
            FileInput {
                path: path.to_string(),
                language,
                content: content.to_string(),
            },
            FileExtraction {
                path: path.to_string(),
                language: Some(language),
                content_hash: hash::sha256_hex(content),
                line_count: content.lines().count() as u32,
                symbols,
                ..Default::default()
            },
        )
    }

    fn imp(spec: &str, names: &[&str]) -> ImportRef {
        ImportRef {
            specifier: spec.into(),
            names: names.iter().map(|s| s.to_string()).collect(),
            line: 1,
        }
    }

    fn call(caller: &str, callee: &str, receiver: Option<&str>) -> CallCandidate {
        CallCandidate {
            caller: caller.into(),
            callee: callee.into(),
            receiver: receiver.map(String::from),
            line: 1,
        }
    }

    fn edges_of(a: &Analyzed, kind: EdgeKind) -> Vec<&Edge> {
        a.bundle.edges.iter().filter(|e| e.kind == kind).collect()
    }

    fn node_by_stable<'a>(a: &'a Analyzed, stable: &str) -> &'a Node {
        a.bundle
            .nodes
            .iter()
            .find(|n| n.stable_id == stable)
            .unwrap()
    }

    fn edge_between(a: &Analyzed, from: &str, to: &str, kind: EdgeKind) -> Option<f32> {
        let f = node_by_stable(a, from).id;
        let t = node_by_stable(a, to).id;
        a.bundle
            .edges
            .iter()
            .find(|e| e.kind == kind && e.source_node_id == f && e.target_node_id == t)
            .map(|e| e.confidence)
    }

    #[test]
    fn rust_crate_paths_resolve_to_files() {
        let mut lib = file("src/lib.rs", "pub mod a; pub mod util;", vec![]);
        lib.1.imports = vec![
            imp("crate::a::b::Thing", &[]), // src/a/b.rs
            imp("crate::util", &[]),        // src/util/mod.rs
            imp("crate::a", &["b", "c"]),   // src/a.rs + names probe b → a/b.rs
            imp("serde::Serialize", &[]),   // bare → dropped
        ];
        lib.1.packages = vec!["serde".into()];
        let mut b = file("src/a/b.rs", "use super::c::X; use self::inner::Y;", vec![]);
        b.1.imports = vec![imp("super::c::X", &[]), imp("self::inner::Y", &[])];
        let files = vec![
            lib,
            file("src/a.rs", "pub mod b; pub mod c;", vec![]),
            b,
            file("src/a/c.rs", "pub struct X;", vec![]),
            file("src/a/b/inner.rs", "pub struct Y;", vec![]),
            file("src/util/mod.rs", "", vec![]),
        ];
        let a = analyze(repo(), &files);
        assert_eq!(a.stats.imports_total, 6);
        assert_eq!(a.stats.imports_resolved, 5);
        assert_eq!(
            edge_between(&a, "file:src/lib.rs", "file:src/a/b.rs", EdgeKind::Imports),
            Some(1.0)
        );
        assert_eq!(
            edge_between(
                &a,
                "file:src/lib.rs",
                "file:src/util/mod.rs",
                EdgeKind::Imports
            ),
            Some(1.0)
        );
        assert_eq!(
            edge_between(&a, "file:src/lib.rs", "file:src/a.rs", EdgeKind::Imports),
            Some(1.0)
        );
        assert_eq!(
            edge_between(&a, "file:src/a/b.rs", "file:src/a/c.rs", EdgeKind::Imports),
            Some(1.0)
        );
        assert_eq!(
            edge_between(
                &a,
                "file:src/a/b.rs",
                "file:src/a/b/inner.rs",
                EdgeKind::Imports
            ),
            Some(1.0)
        );
        // packages land in the file node metadata
        let lib_node = node_by_stable(&a, "file:src/lib.rs");
        assert_eq!(lib_node.metadata["packages"], json!(["serde"]));
        assert_eq!(lib_node.metadata["language"], json!("rust"));
        // repo → file containment
        assert_eq!(edges_of(&a, EdgeKind::Contains).len(), files.len());
        // heads carry the import specifiers in order
        assert_eq!(a.heads[0].imports.len(), 4);
        assert_eq!(a.heads[0].file_index, 0);
    }

    #[test]
    fn ts_relative_imports_probe_extensions_and_index() {
        let mut app = file("src/app.ts", "", vec![]);
        app.1.imports = vec![
            imp("./util", &["helper"]),
            imp("./components", &[]),
            imp("../shared/types.js", &[]),
            imp("react", &[]),
            imp("./missing", &[]),
        ];
        let files = vec![
            app,
            file("src/util.ts", "", vec![]),
            file("src/components/index.tsx", "", vec![]),
            file("shared/types.ts", "", vec![]),
        ];
        let a = analyze(repo(), &files);
        assert_eq!(a.stats.imports_resolved, 3);
        assert!(
            edge_between(&a, "file:src/app.ts", "file:src/util.ts", EdgeKind::Imports).is_some()
        );
        assert!(edge_between(
            &a,
            "file:src/app.ts",
            "file:src/components/index.tsx",
            EdgeKind::Imports
        )
        .is_some());
        assert!(edge_between(
            &a,
            "file:src/app.ts",
            "file:shared/types.ts",
            EdgeKind::Imports
        )
        .is_some());
    }

    #[test]
    fn python_and_solidity_relative_imports() {
        let mut m = file("pkg/sub/mod.py", "", vec![]);
        m.1.imports = vec![
            imp("..core", &["run"]),
            imp("pkg.util", &[]),
            imp(".", &["sibling"]),
        ];
        let mut t = file("contracts/Token.sol", "", vec![]);
        t.1.imports = vec![
            imp("./IERC20.sol", &["IERC20"]),
            imp("@openzeppelin/contracts/x.sol", &[]),
        ];
        let files = vec![
            m,
            file("pkg/core.py", "", vec![]),
            file("pkg/util/__init__.py", "", vec![]),
            file("pkg/sub/sibling.py", "", vec![]),
            t,
            file("contracts/IERC20.sol", "", vec![]),
        ];
        let a = analyze(repo(), &files);
        assert!(edge_between(
            &a,
            "file:pkg/sub/mod.py",
            "file:pkg/core.py",
            EdgeKind::Imports
        )
        .is_some());
        assert!(edge_between(
            &a,
            "file:pkg/sub/mod.py",
            "file:pkg/util/__init__.py",
            EdgeKind::Imports
        )
        .is_some());
        assert!(edge_between(
            &a,
            "file:pkg/sub/mod.py",
            "file:pkg/sub/sibling.py",
            EdgeKind::Imports
        )
        .is_some());
        assert!(edge_between(
            &a,
            "file:contracts/Token.sol",
            "file:contracts/IERC20.sol",
            EdgeKind::Imports
        )
        .is_some());
        assert_eq!(a.stats.imports_resolved, 4);
        assert_eq!(a.stats.imports_total, 5);
    }

    #[test]
    fn call_resolution_rules_a_to_d() {
        // a.rs: fn main calls local `helper` (a), imported `shared` (b), repo-unique `lonely` (d),
        // `dup` defined twice in the same file (dropped), `Vec::new` (dropped: foreign receiver).
        let mut a_rs = file(
            "src/a.rs",
            "use crate::b::shared;\nfn helper() {}\nfn main() { helper(); shared(); lonely(); dup(); Vec::new(); }\n",
            vec![
                sym("rust:src/a.rs:function:helper", NodeKind::Function, "helper", None),
                Symbol { byte_start: 40, byte_end: 116, ..sym("rust:src/a.rs:function:main", NodeKind::Function, "main", None) },
                sym("rust:src/a.rs:function:dup", NodeKind::Function, "dup", None),
                sym("rust:src/a.rs:function:dup#2", NodeKind::Function, "dup", None),
                sym("rust:src/a.rs:module:Foo", NodeKind::Module, "Foo", None),
                sym("rust:src/a.rs:method:Foo::new", NodeKind::Method, "new", Some("rust:src/a.rs:module:Foo")),
                sym("rust:src/a.rs:method:Foo::run", NodeKind::Method, "run", Some("rust:src/a.rs:module:Foo")),
            ],
        );
        a_rs.1.imports = vec![imp("crate::b::shared", &[])];
        a_rs.1.calls = vec![
            call("rust:src/a.rs:function:main", "helper", None),
            call("rust:src/a.rs:function:main", "shared", None),
            call("rust:src/a.rs:function:main", "lonely", None),
            call("rust:src/a.rs:function:main", "dup", None),
            call("rust:src/a.rs:function:main", "new", Some("Vec")),
            // (c) self receiver inside Foo::run → Foo::new
            call("rust:src/a.rs:method:Foo::run", "new", Some("self")),
            // (c) receiver == type name defined elsewhere
            call("rust:src/a.rs:function:main", "start", Some("Engine")),
        ];
        let b_rs = file(
            "src/b.rs",
            "pub fn shared() {}\npub struct Engine; impl Engine { pub fn start() {} pub fn new() {} }",
            vec![
                sym("rust:src/b.rs:function:shared", NodeKind::Function, "shared", None),
                sym("rust:src/b.rs:type:Engine", NodeKind::Type, "Engine", None),
                sym("rust:src/b.rs:module:Engine", NodeKind::Module, "Engine", None),
                sym("rust:src/b.rs:method:Engine::start", NodeKind::Method, "start", Some("rust:src/b.rs:module:Engine")),
                sym("rust:src/b.rs:method:Engine::new", NodeKind::Method, "new", Some("rust:src/b.rs:module:Engine")),
            ],
        );
        let c_rs = file(
            "src/c.rs",
            "pub fn lonely() {}",
            vec![sym(
                "rust:src/c.rs:function:lonely",
                NodeKind::Function,
                "lonely",
                None,
            )],
        );
        let a = analyze(repo(), &[a_rs, b_rs, c_rs]);
        assert_eq!(a.stats.calls_total, 7);
        assert_eq!(a.stats.calls_resolved, 5);
        let main = "rust:src/a.rs:function:main";
        assert_eq!(
            edge_between(&a, main, "rust:src/a.rs:function:helper", EdgeKind::Calls),
            Some(1.0)
        );
        assert_eq!(
            edge_between(&a, main, "rust:src/b.rs:function:shared", EdgeKind::Calls),
            Some(0.9)
        );
        assert_eq!(
            edge_between(&a, main, "rust:src/c.rs:function:lonely", EdgeKind::Calls),
            Some(0.7)
        );
        assert_eq!(
            edge_between(
                &a,
                main,
                "rust:src/b.rs:method:Engine::start",
                EdgeKind::Calls
            ),
            Some(0.8)
        );
        assert_eq!(
            edge_between(
                &a,
                "rust:src/a.rs:method:Foo::run",
                "rust:src/a.rs:method:Foo::new",
                EdgeKind::Calls
            ),
            Some(0.8)
        );
        assert!(edge_between(&a, main, "rust:src/a.rs:function:dup", EdgeKind::Calls).is_none());
        assert!(edge_between(&a, main, "rust:src/a.rs:method:Foo::new", EdgeKind::Calls).is_none());
        // Unit hints: main's calls, sorted; uses = imported `shared` appears in body.
        let main_unit = a.units.iter().find(|u| u.symbol.stable_id == main).unwrap();
        assert_eq!(main_unit.calls, vec!["helper", "lonely", "shared", "start"]);
        assert_eq!(main_unit.uses, vec!["shared"]);
        let run_unit = a
            .units
            .iter()
            .find(|u| u.symbol.stable_id == "rust:src/a.rs:method:Foo::run")
            .unwrap();
        assert_eq!(run_unit.container.as_deref(), Some("Foo"));
        // Containment: file → top-level, container → member.
        assert_eq!(
            edge_between(
                &a,
                "file:src/a.rs",
                "rust:src/a.rs:module:Foo",
                EdgeKind::Contains
            ),
            Some(1.0)
        );
        assert_eq!(
            edge_between(
                &a,
                "rust:src/a.rs:module:Foo",
                "rust:src/a.rs:method:Foo::run",
                EdgeKind::Contains
            ),
            Some(1.0)
        );
        // Module (impl) is not a unit; methods and functions are.
        assert!(a.units.iter().all(|u| u.symbol.kind != NodeKind::Module));
    }

    #[test]
    fn surfaces_are_per_file_nodes_with_defines_edges() {
        let mut a_ts = file(
            "src/a.ts",
            "",
            vec![sym(
                "typescript:src/a.ts:function:boot",
                NodeKind::Function,
                "boot",
                None,
            )],
        );
        a_ts.1.surfaces = vec![
            Surface {
                kind: SurfaceKind::EnvVar,
                name: "DATABASE_URL".into(),
                line: 3,
                owner: Some("typescript:src/a.ts:function:boot".into()),
            },
            Surface {
                kind: SurfaceKind::EnvVar,
                name: "DATABASE_URL".into(),
                line: 9,
                owner: None,
            },
            Surface {
                kind: SurfaceKind::HttpRoute,
                name: "GET /users/:id".into(),
                line: 5,
                owner: None,
            },
        ];
        let mut b_ts = file("src/b.ts", "", vec![]);
        b_ts.1.surfaces = vec![Surface {
            kind: SurfaceKind::EnvVar,
            name: "DATABASE_URL".into(),
            line: 1,
            owner: None,
        }];
        let a = analyze(repo(), &[a_ts, b_ts]);
        let env_a = node_by_stable(&a, "src/a.ts:env:DATABASE_URL");
        let env_b = node_by_stable(&a, "src/b.ts:env:DATABASE_URL");
        assert_ne!(env_a.id, env_b.id);
        assert_eq!(env_a.kind, NodeKind::EnvVar);
        assert_eq!(env_a.line_start, Some(3));
        assert_eq!(
            node_by_stable(&a, "src/a.ts:route:GET /users/:id").kind,
            NodeKind::HttpRoute
        );
        assert_eq!(
            edge_between(
                &a,
                "file:src/a.ts",
                "src/a.ts:env:DATABASE_URL",
                EdgeKind::Defines
            ),
            Some(1.0)
        );
        assert_eq!(
            edge_between(
                &a,
                "typescript:src/a.ts:function:boot",
                "src/a.ts:env:DATABASE_URL",
                EdgeKind::Defines
            ),
            Some(1.0)
        );
        assert_eq!(
            edge_between(
                &a,
                "file:src/b.ts",
                "src/b.ts:env:DATABASE_URL",
                EdgeKind::Defines
            ),
            Some(1.0)
        );
        assert_eq!(
            a.bundle
                .nodes
                .iter()
                .filter(|n| n.kind == NodeKind::EnvVar)
                .count(),
            2
        );
    }

    #[test]
    fn relations_resolve_by_name_and_are_deterministic() {
        let mut ops = file(
            "src/q.graphql",
            "query GetUser { user { id } }",
            vec![sym(
                "graphql:src/q.graphql:operation:GetUser",
                NodeKind::Operation,
                "GetUser",
                None,
            )],
        );
        ops.1.relations = vec![Relation {
            source: "graphql:src/q.graphql:operation:GetUser".into(),
            kind: EdgeKind::Queries,
            target_name: "Query.user".into(),
        }];
        let mut schema = file(
            "schema.graphql",
            "type Query { user: User }",
            vec![
                sym(
                    "graphql:schema.graphql:type:Query",
                    NodeKind::Type,
                    "Query",
                    None,
                ),
                Symbol {
                    qualified_name: "Query.user".into(),
                    ..sym(
                        "graphql:schema.graphql:field:Query.user",
                        NodeKind::Field,
                        "user",
                        Some("graphql:schema.graphql:type:Query"),
                    )
                },
            ],
        );
        schema.1.relations = vec![];
        let mut sol = file(
            "contracts/Token.sol",
            "contract Token is Ownable {}",
            vec![sym(
                "solidity:contracts/Token.sol:type:Token",
                NodeKind::Type,
                "Token",
                None,
            )],
        );
        sol.1.relations = vec![Relation {
            source: "solidity:contracts/Token.sol:type:Token".into(),
            kind: EdgeKind::Extends,
            target_name: "Ownable".into(),
        }];
        let own = file(
            "contracts/Ownable.sol",
            "contract Ownable {}",
            vec![sym(
                "solidity:contracts/Ownable.sol:type:Ownable",
                NodeKind::Type,
                "Ownable",
                None,
            )],
        );
        let files = vec![ops, schema, sol, own];
        let a = analyze(repo(), &files);
        assert_eq!(a.stats.relations_resolved, 2);
        assert_eq!(
            edge_between(
                &a,
                "graphql:src/q.graphql:operation:GetUser",
                "graphql:schema.graphql:field:Query.user",
                EdgeKind::Queries
            ),
            Some(1.0)
        );
        assert_eq!(
            edge_between(
                &a,
                "solidity:contracts/Token.sol:type:Token",
                "solidity:contracts/Ownable.sol:type:Ownable",
                EdgeKind::Extends
            ),
            Some(0.7)
        );
        // Same input → identical bundle (ids + order).
        let b = analyze(repo(), &files);
        assert_eq!(a.bundle.nodes, b.bundle.nodes);
        assert_eq!(a.bundle.edges, b.bundle.edges);
        // No duplicate ids.
        let ids: HashSet<Uuid> = a.bundle.nodes.iter().map(|n| n.id).collect();
        assert_eq!(ids.len(), a.bundle.nodes.len());
        let eids: HashSet<Uuid> = a.bundle.edges.iter().map(|e| e.id).collect();
        assert_eq!(eids.len(), a.bundle.edges.len());
    }

    /// `add` (changed file + persisted graph) must produce the SAME hints and
    /// edge targets as `index` (every file at once) — the chunk header hashes
    /// into content_hash, so any drift re-embeds unchanged code.
    /// `(stable_id, uses, calls, container)` — what feeds the chunk header.
    type UnitHints = (String, Vec<String>, Vec<String>, Option<String>);

    #[test]
    fn analyze_with_known_graph_matches_full_analyze() {
        let a_rs = file(
            "src/a.rs",
            "pub fn helper() {}\npub fn main() {}\n",
            vec![
                sym(
                    "rust:src/a.rs:function:helper",
                    NodeKind::Function,
                    "helper",
                    None,
                ),
                sym(
                    "rust:src/a.rs:function:main",
                    NodeKind::Function,
                    "main",
                    None,
                ),
            ],
        );
        let b_src = "use crate::a::helper;\nuse crate::c;\npub fn shared() { helper(); lonely(); Engine::start(); dup(); Vec::new(); }\npub struct Engine; impl Engine { pub fn start() { self.new(); } pub fn new() {} }\n";
        let mut b_rs = file(
            "src/b.rs",
            b_src,
            vec![
                Symbol {
                    byte_start: 37,
                    byte_end: 116,
                    ..sym(
                        "rust:src/b.rs:function:shared",
                        NodeKind::Function,
                        "shared",
                        None,
                    )
                },
                sym("rust:src/b.rs:type:Engine", NodeKind::Type, "Engine", None),
                sym(
                    "rust:src/b.rs:module:Engine",
                    NodeKind::Module,
                    "Engine",
                    None,
                ),
                sym(
                    "rust:src/b.rs:method:Engine::start",
                    NodeKind::Method,
                    "start",
                    Some("rust:src/b.rs:module:Engine"),
                ),
                sym(
                    "rust:src/b.rs:method:Engine::new",
                    NodeKind::Method,
                    "new",
                    Some("rust:src/b.rs:module:Engine"),
                ),
            ],
        );
        b_rs.1.imports = vec![imp("crate::a::helper", &[]), imp("crate::c", &[])];
        b_rs.1.calls = vec![
            call("rust:src/b.rs:function:shared", "helper", None), // (b) imported a.rs
            call("rust:src/b.rs:function:shared", "lonely", None), // (b) imported c.rs
            call("rust:src/b.rs:function:shared", "start", Some("Engine")), // (c) same file
            call("rust:src/b.rs:function:shared", "dup", None),    // ambiguous: a.rs? no — c.rs x2
            call("rust:src/b.rs:function:shared", "new", Some("Vec")), // foreign
            call("rust:src/b.rs:method:Engine::start", "new", Some("self")), // (c)
            call("rust:src/b.rs:function:shared", "solo", None), // (d) repo-unique, unimported d.rs
        ];
        b_rs.1.relations = vec![Relation {
            source: "rust:src/b.rs:type:Engine".into(),
            kind: EdgeKind::Extends,
            target_name: "Base".into(),
        }];
        let c_rs = file(
            "src/c.rs",
            "pub fn lonely() {}\npub fn dup() {}\npub fn dup() {}\n",
            vec![
                sym(
                    "rust:src/c.rs:function:lonely",
                    NodeKind::Function,
                    "lonely",
                    None,
                ),
                sym(
                    "rust:src/c.rs:function:dup",
                    NodeKind::Function,
                    "dup",
                    None,
                ),
                sym(
                    "rust:src/c.rs:function:dup#2",
                    NodeKind::Function,
                    "dup",
                    None,
                ),
            ],
        );
        let d_rs = file(
            "src/d.rs",
            "pub fn solo() {}\npub trait Base {}\n",
            vec![
                sym(
                    "rust:src/d.rs:function:solo",
                    NodeKind::Function,
                    "solo",
                    None,
                ),
                sym(
                    "rust:src/d.rs:interface:Base",
                    NodeKind::Interface,
                    "Base",
                    None,
                ),
            ],
        );

        let full = analyze(repo(), &[a_rs, b_rs.clone(), c_rs, d_rs]);
        let b_file = node_by_stable(&full, "file:src/b.rs").id;
        let b_node_ids: HashSet<Uuid> = full
            .bundle
            .nodes
            .iter()
            .filter(|n| n.file_id == Some(full.bundle.files[1].id))
            .map(|n| n.id)
            .collect();
        assert!(b_node_ids.contains(&b_file));
        let full_units: Vec<UnitHints> = full
            .units
            .iter()
            .filter(|u| u.file_index == 1)
            .map(|u| {
                (
                    u.symbol.stable_id.clone(),
                    u.uses.clone(),
                    u.calls.clone(),
                    u.container.clone(),
                )
            })
            .collect();
        let shared = full_units
            .iter()
            .find(|u| u.0 == "rust:src/b.rs:function:shared")
            .unwrap();
        assert_eq!(shared.2, vec!["helper", "lonely", "solo", "start"]);
        assert_eq!(shared.1, vec!["helper"]);
        let out_edges = |a: &Analyzed| -> Vec<(Uuid, Uuid, &'static str, u32)> {
            let mut v: Vec<_> = a
                .bundle
                .edges
                .iter()
                .filter(|e| b_node_ids.contains(&e.source_node_id))
                .map(|e| {
                    (
                        e.source_node_id,
                        e.target_node_id,
                        e.kind.as_str(),
                        e.confidence.to_bits(),
                    )
                })
                .collect();
            v.sort();
            v
        };
        let full_edges = out_edges(&full);
        // Every resolution rule is exercised by an edge that leaves b.rs.
        let has = |kind: EdgeKind, conf: f32| {
            full_edges
                .iter()
                .any(|e| e.2 == kind.as_str() && e.3 == conf.to_bits())
        };
        assert!(has(EdgeKind::Calls, CONF_IMPORTED));
        assert!(has(EdgeKind::Calls, CONF_RECEIVER));
        assert!(has(EdgeKind::Calls, CONF_REPO_UNIQUE));
        assert!(has(EdgeKind::Imports, 1.0));
        assert!(has(EdgeKind::Extends, CONF_REPO_UNIQUE));

        // The persisted graph "minus B": what the store holds for the other
        // files (B's rows are purged by merge_files; simulate it here — and
        // ALSO keep B's stale rows to prove they are dropped, not double-counted).
        let mut known = Graph {
            files: full.bundle.files.clone(),
            nodes: full.bundle.nodes.clone(),
            edges: full.bundle.edges.clone(),
        };
        for variant in [true, false] {
            if variant {
                known.files.retain(|f| f.path != "src/b.rs");
                known.nodes.retain(|n| !b_node_ids.contains(&n.id));
                known.edges.retain(|e| {
                    !b_node_ids.contains(&e.source_node_id)
                        && !b_node_ids.contains(&e.target_node_id)
                });
            }
            let inc = analyze_with(repo(), std::slice::from_ref(&b_rs), Some(&known));
            // Bundle carries ONLY b.rs (+ repo node).
            assert_eq!(inc.bundle.files.len(), 1);
            assert!(inc
                .bundle
                .nodes
                .iter()
                .all(|n| n.kind == NodeKind::Repository || b_node_ids.contains(&n.id)));
            assert!(inc.units.iter().all(|u| u.file_index == 0));
            let inc_units: Vec<UnitHints> = inc
                .units
                .iter()
                .map(|u| {
                    (
                        u.symbol.stable_id.clone(),
                        u.uses.clone(),
                        u.calls.clone(),
                        u.container.clone(),
                    )
                })
                .collect();
            assert_eq!(inc_units, full_units);
            assert_eq!(out_edges(&inc), full_edges);
            assert_eq!(inc.stats.calls_resolved, 5);
            assert_eq!(inc.stats.imports_resolved, 2);
            assert_eq!(inc.stats.relations_resolved, 1);
            // Same input → same output.
            let again = analyze_with(repo(), std::slice::from_ref(&b_rs), Some(&known));
            assert_eq!(again.bundle.edges, inc.bundle.edges);
        }
        // Without the known graph the incremental result degrades (the bug).
        let alone = analyze(repo(), std::slice::from_ref(&b_rs));
        assert_ne!(out_edges(&alone), full_edges);
    }

    #[test]
    fn identifier_boundaries() {
        assert!(contains_identifier("let x = shared();", "shared"));
        assert!(!contains_identifier("let x = shared_more();", "shared"));
        assert!(!contains_identifier("unshared()", "shared"));
        assert!(contains_identifier("a.shared.b", "shared"));
        assert_eq!(
            normalize("src/a", "../shared/types.js"),
            "src/shared/types.js"
        );
        assert_eq!(rust_module_dir("src/a/b.rs"), "src/a/b");
        assert_eq!(rust_module_dir("src/a/mod.rs"), "src/a");
        assert_eq!(rust_crate_root("crates/x/src/lib.rs"), "crates/x/src");
        assert_eq!(rust_crate_root("lib.rs"), "");
    }
}
