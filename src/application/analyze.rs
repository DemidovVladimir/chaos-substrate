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
//!   `Queries` and `Binds` (SDL `Field` by qualified name), `Defines` (same
//!   file: the exact stable id, else the one same-named op), `Resolves` (see
//!   below).
//! * Solidity scopes names like solc: the contract itself (1.0), then its
//!   ancestors, most-derived first (≤ 0.9), then file-level declarations
//!   (never another contract's member). That order resolves unqualified
//!   calls, `emit` → `Emits` and `revert` → `Reverts`;
//!   `super.f` binds the nearest ancestor's `f`; `T(x).f` / `T.f` bind `T`'s
//!   own or inherited method; an inherited state variable types its
//!   receiver; an untyped receiver only reaches free functions. A method with
//!   the name and arity of exactly one method of a direct base `Implements`
//!   it. External functions, public getters, errors and events get
//!   `abi_signature` + `selector` / `topic0` when every type resolves.
//! * GraphQL coordinates defined in several SDL files (a merged schema next to
//!   its parts) resolve to ONE canonical copy: the copy whose file defines the
//!   most root fields (tie: smallest path), so `Queries`, `Binds` and
//!   `Resolves` all land on the same `Field`.
//! * `Resolves` (a `Resolver` → the field it implements): a `Type.field`
//!   target (the dispatcher guards on the parent type) → that coordinate
//!   (0.9); a bare field name → among the coordinates of that name the same
//!   resolver's `Type.field` targets do not name, the one some registration
//!   `Binds` (0.9), else the one ROOT coordinate (0.7), else the only one
//!   (0.7); otherwise dropped.
//!   Bindings persisted for unchanged files count too, so `add` matches
//!   `index`. A `Resolver` is never a call or heritage target.
//! * File nodes record `metadata.graphql_bindings` (`[{coordinate, line,
//!   owner}]`) and `metadata.lambda_entries` when the extractor found any.
//! * contract calls → `UsesAbi` edges from the call's owner (else its file)
//!   to the `Abi` definition its `abi` names: (1) a default-imported
//!   `.json`/`.abi` file's ABI → 1.0; (2) by name: same file 1.0 → imported
//!   files 0.9 → repo-unique 0.7; (3) repo-unique after normalisation
//!   (lower-case, `_` and a trailing `abi` removed) → 0.6; otherwise dropped
//!   (an outside ABI such as viem's `erc20Abi`). A combined ABI
//!   (`[...A, ...B] as Abi`) uses each of its parts the same way. The calls
//!   themselves stay raw on the File node (`metadata.contract_calls`).

use crate::application::correlations::is_root_field;
use crate::domain::abi::abi_name_key;
use crate::domain::hash;
use crate::domain::model::{Edge, EdgeKind, IndexBundle, Language, Node, NodeKind, SourceFile};
use crate::domain::solidity::{
    canonical_array_suffix, elementary, split_array_suffix, SolidityLayout,
};
use crate::ports::extractor::{FileExtraction, FileInput, SurfaceKind, Symbol};
use crate::ports::index_store::Graph;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
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
    pub abi_uses_resolved: usize,
    pub abi_uses_total: usize,
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
    /// Solidity external function: `<abi_signature> <selector>`.
    pub abi: Option<String>,
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
/// `Resolves` to a `Type.field` the dispatcher names through a parent-type guard.
const CONF_RESOLVES_TYPED: f32 = 0.9;
/// `Resolves` from a bare field name to the one coordinate a registration binds.
const CONF_RESOLVES_BOUND: f32 = 0.9;
/// `Resolves` from a bare field name to its one root (or only) coordinate.
const CONF_RESOLVES_UNIQUE: f32 = 0.7;
/// An ABI matched only after normalising its name (`AGENTFACTORY` → AgentFactory).
const CONF_ABI_NAME_KEY: f32 = 0.6;

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
    metadata: &'a Value,
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
    analyze_with(repo_id, files, None, &SolidityLayout::default())
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
/// `solidity` (the Foundry projects: remappings, libs) resolves non-relative
/// Solidity imports.
pub fn analyze_with(
    repo_id: Uuid,
    files: &[(FileInput, FileExtraction)],
    known: Option<&Graph>,
    solidity: &SolidityLayout,
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
                    metadata: &s.metadata,
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
    // GraphQL SDL: field name → coordinates, and per file the number of root
    // fields (the canonical-copy rule). Solidity events are `Field`s too.
    let mut sdl_by_field: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    let mut root_count: Vec<usize> = vec![0; view.len()];
    for (fi, f) in view.iter().enumerate() {
        if f.language != Language::GraphQL {
            continue;
        }
        for s in f.symbols.iter().filter(|s| s.kind == NodeKind::Field) {
            sdl_by_field
                .entry(s.name)
                .or_default()
                .insert(s.qualified_name);
            if is_root_field(s.qualified_name) {
                root_count[fi] += 1;
            }
        }
    }
    // Coordinates some resolver registration binds (this run + persisted).
    let mut bound: HashSet<String> = files
        .iter()
        .flat_map(|(_, x)| &x.relations)
        .filter(|r| r.kind == EdgeKind::Binds)
        .map(|r| r.target_name.clone())
        .collect();
    if let Some(g) = known {
        bound.extend(known_bound(g, files));
    }
    let sdl = Sdl {
        fields_by_qualified: &fields_by_qualified,
        by_field: &sdl_by_field,
        root_count: &root_count,
        bound: &bound,
        view: &view,
    };

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
        let sol_packages;
        let packages = if language == Language::Solidity {
            sol_packages = solidity_packages(x, &input.path, &path_index, solidity);
            &sol_packages
        } else {
            &x.packages
        };
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
            metadata: match &x.title {
                Some(t) => json!({
                    "language": language.as_str(),
                    "packages": packages,
                    "line_count": x.line_count,
                    "title": t,
                }),
                None => json!({
                    "language": language.as_str(),
                    "packages": packages,
                    "line_count": x.line_count,
                }),
            },
        });
        // GraphQL resolver registrations, the Lambda handlers this file deploys
        // and its raw contract calls.
        if let Some(Value::Object(m)) = out.bundle.nodes.last_mut().map(|nd| &mut nd.metadata) {
            if !x.contract_calls.is_empty() {
                m.insert("contract_calls".into(), json!(x.contract_calls));
            }
            if !x.graphql_bindings.is_empty() {
                let b: Vec<Value> = x
                    .graphql_bindings
                    .iter()
                    .map(|b| json!({"coordinate": b.coordinate, "line": b.line, "owner": b.owner}))
                    .collect();
                m.insert("graphql_bindings".into(), Value::Array(b));
            }
            if !x.lambda_entries.is_empty() {
                m.insert("lambda_entries".into(), json!(x.lambda_entries));
            }
        }
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
                SurfaceKind::McpTool => ("mcp_tool", NodeKind::McpTool),
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
                    metadata: match &surf.detail {
                        Some(d) => {
                            json!({"path": input.path, "language": language.as_str(), "detail": d})
                        }
                        None => json!({"path": input.path, "language": language.as_str()}),
                    },
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
    // Known files (index ≥ n) take theirs from the persisted `Imports` edges.
    let mut imported_files: Vec<Vec<usize>> = vec![Vec::new(); view.len()];
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
                solidity,
            );
            let targets: Vec<usize> = targets.into_iter().filter(|t| *t != fi).collect();
            if targets.is_empty() {
                continue;
            }
            out.stats.imports_resolved += 1;
            for &t in &targets {
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
            if imp.names.is_empty() && language == Language::Solidity {
                // `import "./Types.sol";` brings every top-level name in.
                for t in &targets {
                    for s in view[*t].symbols.iter().filter(|s| s.parent.is_none()) {
                        push_unique(&mut imported_names[fi], s.name.to_string());
                    }
                }
            } else if imp.names.is_empty() {
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

    if let Some(g) = known {
        known_imports(g, &file_nodes, n, &mut imported_files);
    }
    let sol = Sol::new(&view, &sym_by_id, &typeish_by_name, &imported_files);

    // ---- calls --------------------------------------------------------------
    // resolved callee names per caller stable_id, in source order.
    let mut calls_by_caller: HashMap<&str, Vec<String>> = HashMap::new();
    for (fi, (_, x)) in files.iter().enumerate() {
        for call in &x.calls {
            out.stats.calls_total += 1;
            let resolved = if view[fi].language == Language::Solidity {
                sol.resolve_call(
                    fi,
                    call.caller.as_str(),
                    call.callee.as_str(),
                    call.receiver.as_deref(),
                    &defs_by_name,
                )
            } else {
                resolve_call(
                    fi,
                    call.caller.as_str(),
                    call.callee.as_str(),
                    call.receiver.as_deref(),
                    &view,
                    &sym_by_id,
                    &defs_by_name,
                    &imported_files,
                )
            };
            let Some((target, conf)) = resolved else {
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
                EdgeKind::Queries | EdgeKind::Binds => sdl
                    .canonical(rel.target_name.as_str())
                    .map(|r| (r, CONF_SAME_FILE)),
                EdgeKind::Resolves => {
                    // The coordinates this resolver's guards name are not
                    // what its bare label serves.
                    let guarded: Vec<&str> = x
                        .relations
                        .iter()
                        .filter(|r| {
                            r.kind == EdgeKind::Resolves
                                && r.source == rel.source
                                && r.target_name.contains('.')
                        })
                        .map(|r| r.target_name.as_str())
                        .collect();
                    sdl.resolves(rel.target_name.as_str(), &guarded)
                }
                EdgeKind::Defines => {
                    let ops = || {
                        x.symbols.iter().enumerate().filter(|(_, s)| {
                            matches!(s.kind, NodeKind::Operation | NodeKind::Fragment)
                        })
                    };
                    // The extractor names the exact symbol (`Op#2` is not `Op`);
                    // a bare name must be unique in the file.
                    let exact = ops().find(|(_, s)| s.stable_id == rel.target_name);
                    let cands: Vec<SymRef> = match exact {
                        Some((si, _)) => vec![SymRef { file: fi, sym: si }],
                        None => ops()
                            .filter(|(_, s)| {
                                s.qualified_name == rel.target_name || s.name == rel.target_name
                            })
                            .map(|(si, _)| SymRef { file: fi, sym: si })
                            .collect(),
                    };
                    unique(&cands).map(|r| (r, CONF_SAME_FILE))
                }
                EdgeKind::Emits => {
                    sol.resolve_member(fi, &rel.source, &rel.target_name, &sol.events)
                }
                EdgeKind::Reverts => {
                    sol.resolve_member(fi, &rel.source, &rel.target_name, &sol.errors)
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

    // ---- Solidity: override `Implements`, then the ABI ----------------------
    let mut unit_abi: HashMap<&str, String> = HashMap::new();
    for (fi, (_, x)) in files.iter().enumerate() {
        if view[fi].language != Language::Solidity {
            continue;
        }
        for (si, s) in x.symbols.iter().enumerate() {
            let r = SymRef { file: fi, sym: si };
            for (target, conf) in sol.overridden(r, &defs_by_name) {
                out.stats.relations_resolved += 1;
                let target_id = node_id_of(view[target.file].symbols[target.sym].stable_id);
                edges.push(
                    repo_id,
                    node_id_of(&s.stable_id),
                    target_id,
                    EdgeKind::Implements,
                    conf,
                );
            }
        }
    }
    let node_pos: HashMap<&str, usize> = out
        .bundle
        .nodes
        .iter()
        .enumerate()
        .map(|(i, nd)| (nd.stable_id.as_str(), i))
        .collect();
    let mut abi_writes: Vec<(usize, Value)> = Vec::new();
    for (fi, (_, x)) in files.iter().enumerate() {
        if view[fi].language != Language::Solidity {
            continue;
        }
        for (si, s) in x.symbols.iter().enumerate() {
            let r = SymRef { file: fi, sym: si };
            let Some(item) = sol.abi_item(r) else {
                continue;
            };
            let Some(&pos) = node_pos.get(s.stable_id.as_str()) else {
                continue;
            };
            let fields = match sol.abi(r, item) {
                Ok((sig, hash)) => {
                    if let (AbiItem::Function, NodeKind::Method, Some(h)) = (item, s.kind, &hash) {
                        unit_abi.insert(s.stable_id.as_str(), format!("{sig} {h}"));
                    }
                    match (item, hash) {
                        (AbiItem::Event { .. }, Some(h)) => {
                            json!({"abi_signature": sig, "topic0": h})
                        }
                        (_, Some(h)) => json!({"abi_signature": sig, "selector": h}),
                        (_, None) => json!({"abi_signature": sig}),
                    }
                }
                Err(names) => json!({ "abi_unresolved": names }),
            };
            abi_writes.push((pos, fields));
        }
    }
    for (pos, fields) in abi_writes {
        if let (Value::Object(m), Value::Object(f)) = (&mut out.bundle.nodes[pos].metadata, fields)
        {
            m.extend(f);
        }
    }

    // ---- ABI uses -----------------------------------------------------------
    let mut abis_by_name: HashMap<&str, Vec<SymRef>> = HashMap::new();
    let mut abis_by_key: HashMap<String, Vec<SymRef>> = HashMap::new();
    for (fi, f) in view.iter().enumerate() {
        for (si, s) in f.symbols.iter().enumerate() {
            if s.kind == NodeKind::Abi {
                let r = SymRef { file: fi, sym: si };
                abis_by_name.entry(s.name).or_default().push(r);
                abis_by_key.entry(abi_name_key(s.name)).or_default().push(r);
            }
        }
    }
    let resolve_abi = |fi: usize, name: &str, import: Option<&str>| -> Option<(SymRef, f32)> {
        if let Some(t) = import.and_then(|spec| resolve_ts(spec, view[fi].path, &path_index)) {
            let in_file: Vec<SymRef> = view[t]
                .symbols
                .iter()
                .enumerate()
                .filter(|(_, s)| s.kind == NodeKind::Abi)
                .map(|(si, _)| SymRef { file: t, sym: si })
                .collect();
            if let Some(r) = unique(&in_file) {
                return Some((r, CONF_SAME_FILE));
            }
        }
        if let Some(hit) = abis_by_name
            .get(name)
            .and_then(|c| pick_scoped(c, fi, &imported_files[fi]))
        {
            return Some(hit);
        }
        abis_by_key
            .get(&abi_name_key(name))
            .and_then(|c| unique(c))
            .map(|r| (r, CONF_ABI_NAME_KEY))
    };
    for (fi, (_, x)) in files.iter().enumerate() {
        let mut uses: Vec<(Uuid, &str, Option<&str>)> = Vec::new();
        for call in &x.contract_calls {
            let Some(abi) = call.abi.as_deref() else {
                continue;
            };
            let source = call
                .owner
                .as_deref()
                .filter(|o| sym_by_id.get(o).is_some_and(|r| r.file == fi))
                .map(node_id_of)
                .unwrap_or(file_nodes[fi]);
            uses.push((source, abi, call.abi_import.as_deref()));
        }
        for s in x.symbols.iter().filter(|s| s.kind == NodeKind::Abi) {
            for part in s.metadata["abi"]["combines"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if let Some(name) = part["name"].as_str() {
                    uses.push((node_id_of(&s.stable_id), name, part["import"].as_str()));
                }
            }
        }
        for (source, name, import) in uses {
            out.stats.abi_uses_total += 1;
            let Some((target, conf)) = resolve_abi(fi, name, import) else {
                continue;
            };
            let target_id = node_id_of(view[target.file].symbols[target.sym].stable_id);
            if target_id == source {
                continue;
            }
            out.stats.abi_uses_resolved += 1;
            edges.push(repo_id, source, target_id, EdgeKind::UsesAbi, conf);
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
                abi: unit_abi.get(s.stable_id.as_str()).cloned(),
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
        abi_uses = format!(
            "{}/{}",
            out.stats.abi_uses_resolved, out.stats.abi_uses_total
        ),
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
            metadata: &nd.metadata,
        });
    }
    views
}

/// Coordinates bound by persisted `Binds` edges whose source is NOT in a
/// changed file (a changed file's bindings come from its fresh extraction).
fn known_bound(g: &Graph, changed: &[(FileInput, FileExtraction)]) -> Vec<String> {
    let changed_paths: HashSet<&str> = changed.iter().map(|(f, _)| f.path.as_str()).collect();
    let path_of: HashMap<Uuid, &str> = g.files.iter().map(|f| (f.id, f.path.as_str())).collect();
    let by_id: HashMap<Uuid, &Node> = g.nodes.iter().map(|n| (n.id, n)).collect();
    g.edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Binds)
        .filter_map(|e| {
            let source = by_id.get(&e.source_node_id)?;
            let path = source.file_id.and_then(|f| path_of.get(&f))?;
            if changed_paths.contains(path) {
                return None;
            }
            by_id
                .get(&e.target_node_id)?
                .metadata
                .get("qualified_name")?
                .as_str()
                .map(str::to_string)
        })
        .collect()
}

/// The GraphQL SDL lookups `Queries`/`Binds`/`Resolves` resolve against.
struct Sdl<'v, 'a> {
    fields_by_qualified: &'v HashMap<&'a str, Vec<SymRef>>,
    /// field name → coordinates (GraphQL files only).
    by_field: &'v HashMap<&'a str, BTreeSet<&'a str>>,
    /// per file of the universe: root fields it defines.
    root_count: &'v [usize],
    bound: &'v HashSet<String>,
    view: &'v [FileView<'a>],
}

/// The single element of `v`, if it has exactly one.
fn only<T: Copy>(v: &[T]) -> Option<T> {
    match v {
        [one] => Some(*one),
        _ => None,
    }
}

impl Sdl<'_, '_> {
    /// The canonical copy of a coordinate: the only one, else the copy whose
    /// file defines the most root fields (tie: smallest path).
    fn canonical(&self, coordinate: &str) -> Option<SymRef> {
        match self.fields_by_qualified.get(coordinate)?.as_slice() {
            [] => None,
            [one] => Some(*one),
            many => many.iter().copied().max_by(|a, b| {
                self.root_count[a.file]
                    .cmp(&self.root_count[b.file])
                    .then_with(|| self.view[b.file].path.cmp(self.view[a.file].path))
            }),
        }
    }

    /// A `Resolves` target (see the module docs): never fuzzy, exact names
    /// plus uniqueness. A bare field name is matched among its coordinates
    /// minus `guarded` (the `Type.field` targets of the same resolver).
    fn resolves(&self, target: &str, guarded: &[&str]) -> Option<(SymRef, f32)> {
        if target.contains('.') {
            return self.canonical(target).map(|r| (r, CONF_RESOLVES_TYPED));
        }
        let coords: Vec<&str> = self
            .by_field
            .get(target)?
            .iter()
            .copied()
            .filter(|c| !guarded.contains(c))
            .collect();
        let bound: Vec<&str> = coords
            .iter()
            .copied()
            .filter(|c| self.bound.contains(*c))
            .collect();
        let roots: Vec<&str> = coords
            .iter()
            .copied()
            .filter(|c| is_root_field(c))
            .collect();
        let all = coords;
        let (coordinate, conf) = if let Some(c) = only(&bound) {
            (c, CONF_RESOLVES_BOUND)
        } else if let Some(c) = only(&roots) {
            (c, CONF_RESOLVES_UNIQUE)
        } else if let Some(c) = only(&all) {
            (c, CONF_RESOLVES_UNIQUE)
        } else {
            return None;
        };
        self.canonical(coordinate).map(|r| (r, conf))
    }
}

fn is_symbol_kind(kind: NodeKind) -> bool {
    !matches!(
        kind,
        NodeKind::Repository
            | NodeKind::File
            | NodeKind::EnvVar
            | NodeKind::HttpRoute
            | NodeKind::McpTool
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
        // A dispatcher case sees `this` as its host function does.
        NodeKind::Resolver => container_of(s.parent?, sym_by_id, files),
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

/// Fill `imported_files` for the known files (index ≥ `n`) from the
/// persisted `Imports` edges, so a known file's scope (inherited names, ABI
/// struct members) resolves exactly as in a full analyze.
fn known_imports(g: &Graph, file_nodes: &[Uuid], n: usize, imported_files: &mut [Vec<usize>]) {
    let pos: HashMap<Uuid, usize> = file_nodes
        .iter()
        .enumerate()
        .map(|(i, id)| (*id, i))
        .collect();
    for e in g.edges.iter().filter(|e| e.kind == EdgeKind::Imports) {
        let (Some(&s), Some(&t)) = (pos.get(&e.source_node_id), pos.get(&e.target_node_id)) else {
            continue;
        };
        if s >= n && s != t && !imported_files[s].contains(&t) {
            imported_files[s].push(t);
        }
    }
    for v in imported_files.iter_mut().skip(n) {
        v.sort_unstable();
    }
}

// ---- Solidity ---------------------------------------------------------------

/// A lookup in a contract's scope: one hit, several (overloads; never
/// guessed), or none.
enum Scoped {
    Found(SymRef, f32),
    Ambiguous,
    Missing,
}

/// What a Solidity symbol contributes to the contract ABI.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AbiItem {
    /// External / public function or public getter: `selector`.
    Function,
    /// Custom error: `selector`.
    Error,
    /// Event: `topic0` unless anonymous.
    Event { anonymous: bool },
}

/// One ABI canonicalization (one signature): the type names that did not
/// resolve, the structs on the current expansion path, and the struct tuples
/// already built, keyed by struct and nesting depth.
#[derive(Default)]
struct AbiWalk {
    unresolved: Vec<String>,
    visiting: Vec<SymRef>,
    tuples: HashMap<(SymRef, usize), Option<String>>,
}

/// Struct nesting followed when canonicalizing an ABI type.
const ABI_MAX_DEPTH: usize = 8;
/// Ancestors kept per contract.
const ANCESTORS_CAP: usize = 64;

fn flag(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// A Solidity type as written, for comparing two declarations: whitespace
/// collapsed, array suffix canonical when it can be, and the pure aliases
/// spelled out (`uint` → `uint256`). `address payable` stays itself: outside
/// the ABI it is a different type from `address`.
fn alias_type(ty: &str) -> String {
    let ty = ty.split_whitespace().collect::<Vec<_>>().join(" ");
    let (base, suffix) = split_array_suffix(&ty);
    let suffix = canonical_array_suffix(suffix).unwrap_or_else(|| suffix.to_string());
    let base = match base {
        "address" | "address payable" => base.to_string(),
        _ => elementary(base).unwrap_or_else(|| base.to_string()),
    };
    base + &suffix
}

fn is_identifier(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
}

/// Solidity lookup tables over the whole resolution universe (changed and
/// known files) plus the resolved `is` graph.
struct Sol<'a, 'v> {
    view: &'a [FileView<'v>],
    sym_by_id: &'a HashMap<&'v str, SymRef>,
    imported_files: &'a [Vec<usize>],
    events: HashMap<&'v str, Vec<SymRef>>,
    errors: HashMap<&'v str, Vec<SymRef>>,
    state_vars: HashMap<&'v str, Vec<SymRef>>,
    /// Contracts, interfaces, libraries, structs, enums and user-defined
    /// value types.
    types: HashMap<&'v str, Vec<SymRef>>,
    /// Container → its `is` bases, resolved exactly like its `Extends`
    /// edges, in declaration order.
    bases: HashMap<SymRef, Vec<(SymRef, f32)>>,
    /// Container → every ancestor, most-derived first: the bases in reverse
    /// declaration order, breadth-first (an approximation of C3).
    ancestors: HashMap<SymRef, Vec<SymRef>>,
}

impl<'a, 'v> Sol<'a, 'v> {
    fn new(
        view: &'a [FileView<'v>],
        sym_by_id: &'a HashMap<&'v str, SymRef>,
        typeish_by_name: &HashMap<&'v str, Vec<SymRef>>,
        imported_files: &'a [Vec<usize>],
    ) -> Self {
        let mut sol = Sol {
            view,
            sym_by_id,
            imported_files,
            events: HashMap::new(),
            errors: HashMap::new(),
            state_vars: HashMap::new(),
            types: HashMap::new(),
            bases: HashMap::new(),
            ancestors: HashMap::new(),
        };
        for (fi, f) in view.iter().enumerate() {
            if f.language != Language::Solidity {
                continue;
            }
            for (si, s) in f.symbols.iter().enumerate() {
                let r = SymRef { file: fi, sym: si };
                match s.kind {
                    NodeKind::Field => {
                        let table = if flag(s.metadata, "event") {
                            &mut sol.events
                        } else if flag(s.metadata, "error") {
                            &mut sol.errors
                        } else if flag(s.metadata, "state_var") {
                            &mut sol.state_vars
                        } else {
                            continue;
                        };
                        table.entry(s.name).or_default().push(r);
                    }
                    NodeKind::Type | NodeKind::Interface | NodeKind::Enum | NodeKind::TypeAlias => {
                        sol.types.entry(s.name).or_default().push(r);
                        // Same rule as the `Extends` relation of this base.
                        let bases: Vec<(SymRef, f32)> = s
                            .metadata
                            .get("bases")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .filter_map(|b| {
                                let name = last_segment(b).unwrap_or(b);
                                let cands = typeish_by_name.get(name).map(Vec::as_slice);
                                pick_scoped(cands.unwrap_or(&[]), fi, &imported_files[fi])
                            })
                            .collect();
                        if !bases.is_empty() {
                            sol.bases.insert(r, bases);
                        }
                    }
                    _ => {}
                }
            }
        }
        let containers: Vec<SymRef> = sol.bases.keys().copied().collect();
        for c in containers {
            let mut found: Vec<SymRef> = Vec::new();
            let mut queue: VecDeque<SymRef> = sol.bases[&c].iter().rev().map(|b| b.0).collect();
            while let Some(a) = queue.pop_front() {
                if a == c || found.contains(&a) {
                    continue;
                }
                found.push(a);
                if found.len() >= ANCESTORS_CAP {
                    break;
                }
                if let Some(bs) = sol.bases.get(&a) {
                    queue.extend(bs.iter().rev().map(|b| b.0));
                }
            }
            sol.ancestors.insert(c, found);
        }
        sol
    }

    fn sym(&self, r: SymRef) -> &SymView<'v> {
        &self.view[r.file].symbols[r.sym]
    }

    fn parent(&self, r: SymRef) -> Option<SymRef> {
        self.sym(r)
            .parent
            .and_then(|p| self.sym_by_id.get(p))
            .copied()
    }

    fn parent_name(&self, r: SymRef) -> Option<&'v str> {
        self.parent(r).map(|p| self.sym(p).name)
    }

    fn ancestors_of(&self, c: SymRef) -> &[SymRef] {
        self.ancestors.get(&c).map(Vec::as_slice).unwrap_or(&[])
    }

    fn scope_conf(&self, fi: usize, file: usize) -> f32 {
        if file == fi {
            CONF_SAME_FILE
        } else if self.imported_files[fi].contains(&file) {
            CONF_IMPORTED
        } else {
            CONF_REPO_UNIQUE
        }
    }

    /// `cands` as seen unqualified inside `container`: its own member (1.0),
    /// else the nearest ancestor's (that file's scope confidence, ≤ 0.9).
    fn in_scope(&self, fi: usize, container: Option<SymRef>, cands: &[SymRef]) -> Scoped {
        let Some(c) = container else {
            return Scoped::Missing;
        };
        let owners = std::iter::once(c).chain(self.ancestors_of(c).iter().copied());
        for (i, owner) in owners.enumerate() {
            let hits: Vec<SymRef> = cands
                .iter()
                .copied()
                .filter(|r| self.parent(*r) == Some(owner))
                .collect();
            match hits.as_slice() {
                [] => continue,
                [r] if i == 0 => return Scoped::Found(*r, CONF_SAME_FILE),
                [r] => {
                    return Scoped::Found(*r, self.scope_conf(fi, r.file).min(CONF_IMPORTED));
                }
                _ => return Scoped::Ambiguous,
            }
        }
        Scoped::Missing
    }

    /// The event / error an `emit` / `revert` in `source` names: `Q.N` → `N`
    /// declared in `Q`; else the contract scope, then file-level
    /// declarations (same file → imported → repo-unique). Another
    /// contract's member is not in solc scope, so it is never bound.
    fn resolve_member(
        &self,
        fi: usize,
        source: &str,
        target: &str,
        table: &HashMap<&'v str, Vec<SymRef>>,
    ) -> Option<(SymRef, f32)> {
        if let Some((q, name)) = target.rsplit_once('.') {
            let q = last_segment(q).unwrap_or(q);
            let cands: Vec<SymRef> = table
                .get(name)
                .into_iter()
                .flatten()
                .copied()
                .filter(|r| self.parent_name(*r) == Some(q))
                .collect();
            return pick_scoped(&cands, fi, &self.imported_files[fi]);
        }
        let cands = table.get(target).map(Vec::as_slice).unwrap_or(&[]);
        match self.in_scope(fi, container_of(source, self.sym_by_id, self.view), cands) {
            Scoped::Found(r, c) => return Some((r, c)),
            Scoped::Ambiguous => return None,
            Scoped::Missing => {}
        }
        let top: Vec<SymRef> = cands
            .iter()
            .copied()
            .filter(|r| self.sym(*r).parent.is_none())
            .collect();
        pick_scoped(&top, fi, &self.imported_files[fi])
    }

    /// Solidity call resolution (see the module docs). An unqualified name
    /// is the contract's own or inherited member, else a file-level
    /// declaration (free function, contract or struct); a member of some
    /// other contract is never in scope, so it is never bound.
    fn resolve_call(
        &self,
        fi: usize,
        caller: &str,
        callee: &str,
        receiver: Option<&str>,
        defs_by_name: &HashMap<&str, Vec<SymRef>>,
    ) -> Option<(SymRef, f32)> {
        let defs = defs_by_name.get(callee).map(Vec::as_slice).unwrap_or(&[]);
        let container = container_of(caller, self.sym_by_id, self.view);
        let methods: Vec<SymRef> = defs
            .iter()
            .copied()
            .filter(|r| self.sym(*r).kind == NodeKind::Method)
            .collect();
        // Same file (1.0) → imported (0.9), plus repo-unique (0.7) when `repo`.
        let top_level = |kinds: &[NodeKind], repo: bool| {
            let top: Vec<SymRef> = defs
                .iter()
                .copied()
                .filter(|r| {
                    let s = self.sym(*r);
                    s.parent.is_none() && kinds.contains(&s.kind)
                })
                .collect();
            match pick_scoped(&top, fi, &self.imported_files[fi]) {
                Some((_, c)) if c == CONF_REPO_UNIQUE && !repo => None,
                hit => hit,
            }
        };
        match receiver.map(str::trim).filter(|r| !r.is_empty()) {
            None => match self.in_scope(fi, container, defs) {
                Scoped::Found(r, c) => Some((r, c)),
                Scoped::Ambiguous => None,
                Scoped::Missing => top_level(
                    &[NodeKind::Function, NodeKind::Type, NodeKind::Interface],
                    true,
                ),
            },
            Some("this") => match self.in_scope(fi, container, &methods) {
                Scoped::Found(r, _) => Some((r, CONF_RECEIVER)),
                _ => None,
            },
            Some("super") => {
                for &a in self.ancestors_of(container?) {
                    let hits: Vec<SymRef> = methods
                        .iter()
                        .copied()
                        .filter(|r| self.parent(*r) == Some(a))
                        .collect();
                    match hits.as_slice() {
                        [] => continue,
                        [r] => return Some((*r, CONF_RECEIVER)),
                        _ => return None,
                    }
                }
                None
            }
            Some(recv) => {
                // `self` in a library: the library's own function.
                if recv == "self" {
                    match self.in_scope(fi, container, &methods) {
                        Scoped::Found(r, _) => return Some((r, CONF_RECEIVER)),
                        Scoped::Ambiguous => return None,
                        Scoped::Missing => {}
                    }
                }
                if let Some(hit) = self.member_of(fi, recv, &methods) {
                    return hit;
                }
                if is_identifier(recv) && !recv.starts_with(|c: char| c.is_ascii_uppercase()) {
                    if let Some(hit) = self.inherited_state_var_member(container, recv, &methods) {
                        return hit;
                    }
                }
                if recv.starts_with(|c: char| c.is_ascii_uppercase()) {
                    return None;
                }
                // An untyped value: only a free function (`using {f} for T`)
                // can be called on it.
                top_level(&[NodeKind::Function], false)
            }
        }
    }

    /// `T.f` / `T(x).f`: `T`'s own method `f`, else its nearest ancestor's.
    /// `None` when `T` names no known type; `Some(None)` when it does but
    /// the method is missing or ambiguous (never bound elsewhere).
    fn member_of(&self, fi: usize, ty: &str, methods: &[SymRef]) -> Option<Option<(SymRef, f32)>> {
        let name = last_segment(ty).unwrap_or(ty);
        let direct: Vec<SymRef> = methods
            .iter()
            .copied()
            .filter(|r| self.parent_name(*r) == Some(name))
            .collect();
        if !direct.is_empty() {
            return Some(
                pick_scoped(&direct, fi, &self.imported_files[fi]).map(|(r, _)| (r, CONF_RECEIVER)),
            );
        }
        let containers: Vec<SymRef> = self
            .types
            .get(name)
            .into_iter()
            .flatten()
            .copied()
            .filter(|r| {
                let s = self.sym(*r);
                matches!(s.kind, NodeKind::Type | NodeKind::Interface)
                    && !flag(s.metadata, "struct")
            })
            .collect();
        let (t, _) = pick_scoped(&containers, fi, &self.imported_files[fi])?;
        for &a in self.ancestors_of(t) {
            let hits: Vec<SymRef> = methods
                .iter()
                .copied()
                .filter(|r| self.parent(*r) == Some(a))
                .collect();
            match hits.as_slice() {
                [] => continue,
                [r] => return Some(Some((*r, CONF_RECEIVER))),
                _ => return Some(None),
            }
        }
        Some(None)
    }

    /// `token.f()` where `token` is a state variable of the caller's
    /// contract or one of its ancestors, declared with a contract,
    /// interface or library type.
    fn inherited_state_var_member(
        &self,
        container: Option<SymRef>,
        recv: &str,
        methods: &[SymRef],
    ) -> Option<Option<(SymRef, f32)>> {
        let c = container?;
        let vars = self.state_vars.get(recv)?;
        let owners = std::iter::once(c).chain(self.ancestors_of(c).iter().copied());
        for owner in owners {
            let hits: Vec<SymRef> = vars
                .iter()
                .copied()
                .filter(|r| self.parent(*r) == Some(owner))
                .collect();
            match hits.as_slice() {
                [] => continue,
                [v] => {
                    let ty = self.sym(*v).metadata.get("type").and_then(Value::as_str)?;
                    let plain = ty.split('.').all(is_identifier);
                    return if plain {
                        self.member_of(v.file, ty, methods)
                    } else {
                        None
                    };
                }
                _ => return None,
            }
        }
        None
    }

    /// Direct bases' methods that the Solidity method (or public state
    /// variable getter) `r` overrides or implements: the one method of that
    /// base with the same name and the same parameter types (see
    /// [`Sol::same_parameters`]), at the confidence of the `Extends` edge.
    fn overridden(
        &self,
        r: SymRef,
        defs_by_name: &HashMap<&str, Vec<SymRef>>,
    ) -> Vec<(SymRef, f32)> {
        let s = self.sym(r);
        // A public state variable implements a base function through its getter.
        let getter = s.kind == NodeKind::Field && flag(s.metadata, "getter");
        if s.kind != NodeKind::Method && !getter {
            return Vec::new();
        }
        let Some(bases) = self.parent(r).and_then(|c| self.bases.get(&c)) else {
            return Vec::new();
        };
        let defs = defs_by_name.get(s.name).map(Vec::as_slice).unwrap_or(&[]);
        let mut out = Vec::new();
        for &(base, conf) in bases {
            let hits: Vec<SymRef> = defs
                .iter()
                .copied()
                .filter(|d| self.sym(*d).kind == NodeKind::Method && self.parent(*d) == Some(base))
                .filter(|d| self.same_parameters(*d, r))
                .collect();
            if let [d] = hits.as_slice() {
                out.push((*d, conf));
            }
        }
        out
    }

    /// Whether two Solidity functions (or getters) take the same parameter
    /// types, as an override must: equal ABI signatures when both are in the
    /// ABI and resolve, else equal written types after whitespace and the
    /// `uint`/`int`/`byte`/`fixed`/`ufixed` aliases are normalized. A
    /// same-arity overload never matches; neither does a side without
    /// `params`.
    fn same_parameters(&self, a: SymRef, b: SymRef) -> bool {
        let abi = |r: SymRef| {
            self.abi_item(r)
                .and_then(|item| self.abi(r, item).ok())
                .map(|(sig, _)| sig)
        };
        if let (Some(x), Some(y)) = (abi(a), abi(b)) {
            return x == y;
        }
        let written = |r: SymRef| -> Option<Vec<String>> {
            let params = self.sym(r).metadata.get("params")?.as_array()?;
            let ty = |p: &Value| p.get("type").and_then(Value::as_str).map(alias_type);
            params.iter().map(ty).collect()
        };
        matches!((written(a), written(b)), (Some(x), Some(y)) if x == y)
    }

    /// The ABI item `r` declares: external / public functions and public
    /// getters of contracts and interfaces (not libraries), custom errors
    /// and events.
    fn abi_item(&self, r: SymRef) -> Option<AbiItem> {
        let s = self.sym(r);
        let m = s.metadata;
        let in_contract = || {
            self.parent(r).is_some_and(|p| {
                let ps = self.sym(p);
                matches!(ps.kind, NodeKind::Type | NodeKind::Interface)
                    && !flag(ps.metadata, "library")
                    && !flag(ps.metadata, "struct")
            })
        };
        match s.kind {
            NodeKind::Field if flag(m, "event") => Some(AbiItem::Event {
                anonymous: flag(m, "anonymous"),
            }),
            NodeKind::Field if flag(m, "error") => Some(AbiItem::Error),
            NodeKind::Field if flag(m, "getter") => in_contract().then_some(AbiItem::Function),
            NodeKind::Method => {
                let external = matches!(
                    m.get("visibility").and_then(Value::as_str),
                    Some("public" | "external")
                );
                let special = s.name == "constructor" || m.get("special").is_some();
                (external && !special && in_contract()).then_some(AbiItem::Function)
            }
            _ => None,
        }
    }

    /// `(abi_signature, selector or topic0)`, or the type names that did not
    /// resolve (then no signature is written: never guessed).
    fn abi(&self, r: SymRef, item: AbiItem) -> Result<(String, Option<String>), Vec<String>> {
        let s = self.sym(r);
        let scope = self.parent(r);
        let mut walk = AbiWalk::default();
        let mut types: Vec<String> = Vec::new();
        let mut complete = true;
        for p in s
            .metadata
            .get("params")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let ty = p.get("type").and_then(Value::as_str).unwrap_or("");
            match self.abi_type(r.file, scope, ty, 0, &mut walk) {
                Some(t) => types.push(t),
                None => complete = false,
            }
        }
        if !complete || !walk.unresolved.is_empty() {
            return Err(walk.unresolved);
        }
        let sig = format!("{}({})", s.name, types.join(","));
        let hash = match item {
            AbiItem::Function | AbiItem::Error => Some(hash::selector_hex(&sig)),
            AbiItem::Event { anonymous: false } => Some(hash::topic_hex(&sig)),
            AbiItem::Event { anonymous: true } => None,
        };
        Ok((sig, hash))
    }

    /// Canonical ABI type of `ty` as written in `file` inside `scope` (the
    /// declaring contract): `uint` → `uint256`, contracts and interfaces →
    /// `address`, enums → `uint8`, user-defined value types → their
    /// underlying type, structs → `(…)` tuples; array suffixes kept. A type
    /// that does not resolve is recorded in `walk.unresolved`.
    fn abi_type(
        &self,
        file: usize,
        scope: Option<SymRef>,
        ty: &str,
        depth: usize,
        walk: &mut AbiWalk,
    ) -> Option<String> {
        let ty = ty.split_whitespace().collect::<Vec<_>>().join(" ");
        if ty == "function" || ty.starts_with("function ") || ty.starts_with("function(") {
            return Some("function".to_string());
        }
        let (base, suffix) = split_array_suffix(&ty);
        let suffix = canonical_array_suffix(suffix).filter(|_| !base.starts_with("mapping"));
        let Some(suffix) = suffix else {
            push_unique(&mut walk.unresolved, ty.clone());
            return None;
        };
        if let Some(e) = elementary(base) {
            return Some(e + &suffix);
        }
        let Some(r) = self.resolve_type(file, scope, base) else {
            push_unique(&mut walk.unresolved, base.to_string());
            return None;
        };
        let s = self.sym(r);
        let canonical = match s.kind {
            NodeKind::Interface => Some("address".to_string()),
            NodeKind::Enum => Some("uint8".to_string()),
            NodeKind::TypeAlias => s
                .metadata
                .get("underlying")
                .and_then(Value::as_str)
                .and_then(elementary),
            NodeKind::Type if flag(s.metadata, "struct") && depth < ABI_MAX_DEPTH => {
                if walk.visiting.contains(&r) {
                    // A recursive struct has no ABI encoding: never expanded.
                    None
                } else {
                    return self.struct_tuple(r, depth, walk).map(|t| t + &suffix);
                }
            }
            NodeKind::Type if !flag(s.metadata, "struct") && !flag(s.metadata, "library") => {
                Some("address".to_string())
            }
            _ => None,
        };
        match canonical {
            Some(c) => Some(c + &suffix),
            None => {
                push_unique(&mut walk.unresolved, base.to_string());
                None
            }
        }
    }

    /// The `(…)` tuple of struct `r` at nesting `depth`, built once per
    /// [`AbiWalk`]: `r` stays on the visiting path while its members are
    /// canonicalized, so a struct that contains itself fails instead of
    /// expanding until [`ABI_MAX_DEPTH`]. `None` when a member does not
    /// resolve (that member's name is in `walk.unresolved`).
    fn struct_tuple(&self, r: SymRef, depth: usize, walk: &mut AbiWalk) -> Option<String> {
        if let Some(t) = walk.tuples.get(&(r, depth)) {
            return t.clone();
        }
        walk.visiting.push(r);
        let mut members: Vec<String> = Vec::new();
        let mut complete = true;
        for m in self
            .sym(r)
            .metadata
            .get("members")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let t = m.get("type").and_then(Value::as_str).unwrap_or("");
            match self.abi_type(r.file, self.parent(r), t, depth + 1, walk) {
                Some(t) => members.push(t),
                None => complete = false,
            }
        }
        walk.visiting.pop();
        let tuple = complete.then(|| format!("({})", members.join(",")));
        walk.tuples.insert((r, depth), tuple.clone());
        tuple
    }

    /// A user-defined type name: `Q.N` → `N` declared in `Q`; else the
    /// contract scope, then file-level declarations (same file → imported
    /// → repo-unique).
    fn resolve_type(&self, file: usize, scope: Option<SymRef>, base: &str) -> Option<SymRef> {
        let (qual, name) = match base.rsplit_once('.') {
            Some((q, n)) => (Some(last_segment(q).unwrap_or(q)), n),
            None => (None, base),
        };
        let cands = self.types.get(name).map(Vec::as_slice).unwrap_or(&[]);
        if let Some(q) = qual {
            let hits: Vec<SymRef> = cands
                .iter()
                .copied()
                .filter(|r| self.parent_name(*r) == Some(q))
                .collect();
            return pick_scoped(&hits, file, &self.imported_files[file]).map(|(r, _)| r);
        }
        match self.in_scope(file, scope, cands) {
            Scoped::Found(r, _) => return Some(r),
            Scoped::Ambiguous => return None,
            Scoped::Missing => {}
        }
        let top: Vec<SymRef> = cands
            .iter()
            .copied()
            .filter(|r| self.sym(*r).parent.is_none())
            .collect();
        pick_scoped(&top, file, &self.imported_files[file]).map(|(r, _)| r)
    }
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
    solidity: &SolidityLayout,
) -> Vec<usize> {
    match language {
        Language::Rust => resolve_rust(specifier, names, cur_path, paths),
        Language::TypeScript | Language::JavaScript => {
            resolve_ts(specifier, cur_path, paths).into_iter().collect()
        }
        Language::Python => resolve_python(specifier, names, cur_path, paths),
        Language::Solidity => resolve_solidity(specifier, cur_path, paths, solidity)
            .into_iter()
            .collect(),
        Language::GraphQL | Language::Markdown | Language::Transcript | Language::Abi => Vec::new(),
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

/// solc's lookup, in order: a `.`-relative path; a remapped path (a
/// remapped import never falls through); the project dir as base path
/// (`src/X.sol`); each include path (`lib/`, `node_modules/`).
fn resolve_solidity(
    spec: &str,
    cur_path: &str,
    paths: &HashMap<&str, usize>,
    layout: &SolidityLayout,
) -> Option<usize> {
    if spec.starts_with('.') {
        return paths
            .get(normalize(dir_of(cur_path), spec).as_str())
            .copied();
    }
    if let Some((target, _)) = layout.remap(cur_path, spec) {
        return paths.get(target.as_str()).copied();
    }
    if let Some(&i) = paths.get(normalize(layout.base_dir(cur_path), spec).as_str()) {
        return Some(i);
    }
    layout
        .lib_dirs(cur_path)
        .iter()
        .find_map(|lib| paths.get(normalize(lib, spec).as_str()).copied())
}

/// Packages a Solidity file depends on: every non-relative import that does
/// not resolve to a repository file, named by the remapping key it went
/// through (`@openzeppelin-contracts-5.6.0`, `forge-std`), else by `@scope/name`
/// or its first path segment.
fn solidity_packages(
    x: &FileExtraction,
    cur_path: &str,
    paths: &HashMap<&str, usize>,
    layout: &SolidityLayout,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for imp in &x.imports {
        let spec = imp.specifier.as_str();
        if spec.is_empty()
            || spec.starts_with('.')
            || resolve_solidity(spec, cur_path, paths, layout).is_some()
        {
            continue;
        }
        let pkg = match layout.remap(cur_path, spec) {
            Some((_, r)) => r.prefix.trim_end_matches('/').to_string(),
            None => {
                let mut segs = spec.split('/');
                let first = segs.next().unwrap_or("");
                match (first.starts_with('@'), segs.next()) {
                    (true, Some(second)) => format!("{first}/{second}"),
                    (true, None) => continue,
                    (false, _) => first.to_string(),
                }
            }
        };
        if !pkg.is_empty() && !out.contains(&pkg) {
            out.push(pkg);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::solidity::{FoundryProject, Remapping};
    use crate::ports::extractor::{
        AbiScope, CallCandidate, ContractCall, ContractMemberKind, GraphqlBinding, ImportRef,
        Relation, Surface,
    };

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
                detail: None,
            },
            Surface {
                kind: SurfaceKind::EnvVar,
                name: "DATABASE_URL".into(),
                line: 9,
                owner: None,
                detail: None,
            },
            Surface {
                kind: SurfaceKind::HttpRoute,
                name: "GET /users/:id".into(),
                line: 5,
                owner: None,
                detail: None,
            },
        ];
        let mut b_ts = file("src/b.ts", "", vec![]);
        b_ts.1.surfaces = vec![Surface {
            kind: SurfaceKind::EnvVar,
            name: "DATABASE_URL".into(),
            line: 1,
            owner: None,
            detail: None,
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

    /// Two same-named embedded operations in one host file (`Op`, `Op#2`):
    /// `Defines` by stable id gives each host its own copy; a bare name that
    /// two copies share is a tie and stays unresolved, never guessed.
    #[test]
    fn defines_targets_the_exact_embedded_operation() {
        let op = |id: &str, line: u32| Symbol {
            qualified_name: "GetServiceSignInMessage".into(),
            line_start: line,
            line_end: line,
            ..sym(id, NodeKind::Operation, "GetServiceSignInMessage", None)
        };
        const OP1: &str = "graphql:mcp/server.py:operation:GetServiceSignInMessage";
        const OP2: &str = "graphql:mcp/server.py:operation:GetServiceSignInMessage#2";
        const HOST1: &str = "python:mcp/server.py:function:issue_service_token";
        const HOST2: &str = "python:mcp/server.py:function:issue_owner_service_token";
        let mut server = file(
            "mcp/server.py",
            "def issue_service_token(): ...\ndef issue_owner_service_token(): ...\n",
            vec![
                sym(HOST1, NodeKind::Function, "issue_service_token", None),
                op(OP1, 1),
                sym(HOST2, NodeKind::Function, "issue_owner_service_token", None),
                op(OP2, 2),
            ],
        );
        let defines = |source: &str, target: &str| Relation {
            source: source.into(),
            kind: EdgeKind::Defines,
            target_name: target.into(),
        };
        server.1.relations = vec![
            defines(HOST1, OP1),
            defines(HOST2, OP2),
            defines("file:mcp/server.py", "GetServiceSignInMessage"),
        ];
        let a = analyze(repo(), &[server]);
        assert_eq!(
            edge_between(&a, HOST1, OP1, EdgeKind::Defines),
            Some(CONF_SAME_FILE)
        );
        assert_eq!(
            edge_between(&a, HOST2, OP2, EdgeKind::Defines),
            Some(CONF_SAME_FILE)
        );
        assert_eq!(edge_between(&a, HOST1, OP2, EdgeKind::Defines), None);
        assert_eq!(edge_between(&a, HOST2, OP1, EdgeKind::Defines), None);
        // The bare shared name matches both copies: dropped.
        for target in [OP1, OP2] {
            assert_eq!(
                edge_between(&a, "file:mcp/server.py", target, EdgeKind::Defines),
                None
            );
        }
        assert_eq!(a.stats.relations_resolved, 2);
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
            let inc = analyze_with(
                repo(),
                std::slice::from_ref(&b_rs),
                Some(&known),
                &SolidityLayout::default(),
            );
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
            let again = analyze_with(
                repo(),
                std::slice::from_ref(&b_rs),
                Some(&known),
                &SolidityLayout::default(),
            );
            assert_eq!(again.bundle.edges, inc.bundle.edges);
        }
        // Without the known graph the incremental result degrades (the bug).
        let alone = analyze(repo(), std::slice::from_ref(&b_rs));
        assert_ne!(out_edges(&alone), full_edges);
    }

    // ---- Solidity ----------------------------------------------------------
    // Fixtures model onchainlabs / ocltokenizer / labs-contracts declarations
    // as the Solidity extractor emits them (stable ids, metadata, relations).

    /// A Solidity symbol: `id` is `solidity:<path>:<kind>:<Qualified::name>`.
    fn sol(id: &str, kind: NodeKind, parent: Option<&str>, metadata: Value) -> Symbol {
        let qualified = id.splitn(4, ':').nth(3).unwrap();
        let name = qualified.rsplit("::").next().unwrap();
        Symbol {
            qualified_name: qualified.to_string(),
            metadata,
            ..sym(id, kind, name, parent)
        }
    }

    fn p(name: &str, ty: &str) -> Value {
        json!({"name": name, "type": ty, "location": ""})
    }

    // ---- GraphQL: canonical SDL copy, Binds, Resolves ------------------------

    fn field(path: &str, coordinate: &str) -> Symbol {
        let (ty, name) = coordinate.split_once('.').unwrap();
        Symbol {
            qualified_name: coordinate.into(),
            ..sym(
                &format!("graphql:{path}:field:{coordinate}"),
                NodeKind::Field,
                name,
                Some(&format!("graphql:{path}:type:{ty}")),
            )
        }
    }

    fn sdl(path: &str, coordinates: &[&str]) -> (FileInput, FileExtraction) {
        let mut types: Vec<&str> = coordinates
            .iter()
            .map(|c| c.split_once('.').unwrap().0)
            .collect();
        types.dedup();
        let mut symbols: Vec<Symbol> = types
            .iter()
            .map(|t| sym(&format!("graphql:{path}:type:{t}"), NodeKind::Type, t, None))
            .collect();
        symbols.extend(coordinates.iter().map(|c| field(path, c)));
        file(path, "type Mutation { x: Int }", symbols)
    }

    fn rel(source: &str, kind: EdgeKind, target: &str) -> Relation {
        Relation {
            source: source.into(),
            kind,
            target_name: target.into(),
        }
    }

    const I_LAB: &str = "solidity:src/interfaces/ILabNFT.sol:interface:ILabNFT";
    const I_LAB_NOTIFY: &str =
        "solidity:src/interfaces/ILabNFT.sol:method:ILabNFT::notifyIdentityCreated";
    const I_LAB_MINT: &str = "solidity:src/interfaces/ILabNFT.sol:field:ILabNFT::Mint";
    const I_LAB_FEE: &str = "solidity:src/interfaces/ILabNFT.sol:method:ILabNFT::mintFeeWei";
    const BASE: &str = "solidity:src/NFT/ERC721Base.sol:type:ERC721Base";
    const BASE_HOOK: &str =
        "solidity:src/NFT/ERC721Base.sol:method:ERC721Base::_beforeTokenTransfer";
    const LAB: &str = "solidity:src/NFT/LabNFT.sol:type:LabNFT";
    const LAB_MINT: &str = "solidity:src/NFT/LabNFT.sol:method:LabNFT::mint";
    const LAB_NOTIFY: &str = "solidity:src/NFT/LabNFT.sol:method:LabNFT::notifyIdentityCreated";
    const LAB_HOOK: &str = "solidity:src/NFT/LabNFT.sol:method:LabNFT::_beforeTokenTransfer";
    const LAB_FEE: &str = "solidity:src/NFT/LabNFT.sol:field:LabNFT::mintFeeWei";
    const LAB_DID: &str = "solidity:src/NFT/LabNFT.sol:field:LabNFT::activeDidHash";
    const I_OCL: &str = "solidity:src/interfaces/IOnChainLab.sol:interface:IOnChainLab";
    const I_OCL_CALLER: &str =
        "solidity:src/interfaces/IOnChainLab.sol:field:IOnChainLab::InvalidCaller";
    const I_OCL_NOT_INIT: &str =
        "solidity:src/interfaces/IOnChainLab.sol:field:IOnChainLab::NotInitialized";
    const I_MOD_NOT_INIT: &str =
        "solidity:src/interfaces/IERC7579Modules.sol:field:IModule::NotInitialized";
    const OCL: &str = "solidity:src/OnChainLab.sol:type:OnChainLab";
    const OCL_GUARD: &str = "solidity:src/OnChainLab.sol:function:OnChainLab::whenInitialized";
    const OCL_VALIDATE: &str = "solidity:src/OnChainLab.sol:method:OnChainLab::validateUserOp";
    const OCL_POLICY: &str = "solidity:src/OnChainLab.sol:method:OnChainLab::setPolicy";
    const OCL_EXECUTE: &str = "solidity:src/OnChainLab.sol:method:OnChainLab::execute";
    const OCL_INTERNAL: &str = "solidity:src/OnChainLab.sol:method:OnChainLab::_validate";
    const OCL_WITHDRAW: &str = "solidity:src/OnChainLab.sol:method:OnChainLab::withdraw";
    const ERRORS: &str = "solidity:src/libraries/Errors.sol:type:Errors";
    const ERRORS_ZERO: &str = "solidity:src/libraries/Errors.sol:field:Errors::Zero";
    const ERRORS_CHECK: &str = "solidity:src/libraries/Errors.sol:method:Errors::check";
    const TYPES_ZERO: &str = "solidity:src/types/Types.sol:field:Zero";
    const TOK_MUST: &str = "solidity:src/OclTokenizer.sol:field:MustControlLab";
    const TOK_TOKENIZE: &str = "solidity:src/OclTokenizer.sol:method:OclTokenizer::tokenize";
    const FACTORY_BASE: &str = "solidity:src/factory/FactoryBase.sol:type:FactoryBase";
    const FACTORY: &str = "solidity:src/factory/OnChainLabFactory.sol:type:OnChainLabFactory";
    const FACTORY_CREATE: &str =
        "solidity:src/factory/OnChainLabFactory.sol:method:OnChainLabFactory::createLab";
    const FACTORY_NOTIFY: &str =
        "solidity:src/factory/OnChainLabFactory.sol:method:OnChainLabFactory::notify";
    const FACTORY_REGISTER: &str =
        "solidity:src/factory/OnChainLabFactory.sol:method:OnChainLabFactory::register";

    fn sol_fixture() -> Vec<(FileInput, FileExtraction)> {
        let ilab = file(
            "src/interfaces/ILabNFT.sol",
            "",
            vec![
                sol(I_LAB, NodeKind::Interface, None, json!({})),
                sol(
                    I_LAB_MINT,
                    NodeKind::Field,
                    Some(I_LAB),
                    json!({"event": true, "params": [
                        {"name": "to", "type": "address", "indexed": true},
                        {"name": "tokenId", "type": "uint256", "indexed": true}]}),
                ),
                sol(
                    I_LAB_NOTIFY,
                    NodeKind::Method,
                    Some(I_LAB),
                    json!({"visibility": "external", "params": [p("tokenId", "uint256")], "returns": []}),
                ),
                sol(
                    I_LAB_FEE,
                    NodeKind::Method,
                    Some(I_LAB),
                    json!({"visibility": "external", "mutability": "view", "params": [],
                        "returns": [{"name": "", "type": "uint256"}]}),
                ),
            ],
        );
        let base = file(
            "src/NFT/ERC721Base.sol",
            "",
            vec![
                sol(BASE, NodeKind::Type, None, json!({"abstract": true})),
                sol(
                    BASE_HOOK,
                    NodeKind::Method,
                    Some(BASE),
                    json!({"visibility": "internal", "virtual": true, "params": [
                        p("from", "address"), p("to", "address"), p("tokenId", "uint256")]}),
                ),
            ],
        );
        let mut lab = file(
            "src/NFT/LabNFT.sol",
            "",
            vec![
                sol(
                    LAB,
                    NodeKind::Type,
                    None,
                    json!({"bases": ["ERC721Base", "ILabNFT"]}),
                ),
                sol(
                    LAB_FEE,
                    NodeKind::Field,
                    Some(LAB),
                    json!({"state_var": true, "visibility": "public", "getter": true,
                        "mutability": "view", "type": "uint256", "params": [],
                        "returns": [{"name": "", "type": "uint256"}]}),
                ),
                sol(
                    LAB_DID,
                    NodeKind::Field,
                    Some(LAB),
                    json!({"state_var": true, "visibility": "public", "getter": true,
                        "mutability": "view",
                        "type": "mapping(bytes32 oclId => mapping(bytes32 provider => mapping(bytes32 subject => bytes32)))",
                        "params": [
                            {"name": "oclId", "type": "bytes32"},
                            {"name": "provider", "type": "bytes32"},
                            {"name": "subject", "type": "bytes32"}],
                        "returns": [{"name": "", "type": "bytes32"}]}),
                ),
                sol(
                    LAB_MINT,
                    NodeKind::Method,
                    Some(LAB),
                    json!({"visibility": "public", "params": [p("to", "address")], "returns": []}),
                ),
                sol(
                    LAB_NOTIFY,
                    NodeKind::Method,
                    Some(LAB),
                    json!({"visibility": "external", "override": true,
                        "params": [p("tokenId", "uint256")], "returns": []}),
                ),
                sol(
                    LAB_HOOK,
                    NodeKind::Method,
                    Some(LAB),
                    json!({"visibility": "internal", "override": true, "params": [
                        p("from", "address"), p("to", "address"), p("tokenId", "uint256")]}),
                ),
            ],
        );
        lab.1.imports = vec![
            imp("src/interfaces/ILabNFT.sol", &["ILabNFT"]),
            imp("./ERC721Base.sol", &["ERC721Base"]),
        ];
        lab.1.relations = vec![
            rel(LAB, EdgeKind::Extends, "ERC721Base"),
            rel(LAB, EdgeKind::Extends, "ILabNFT"),
            rel(LAB_MINT, EdgeKind::Emits, "Mint"),
        ];
        lab.1.calls = vec![call(LAB_HOOK, "_beforeTokenTransfer", Some("super"))];

        let iocl = file(
            "src/interfaces/IOnChainLab.sol",
            "",
            vec![
                sol(I_OCL, NodeKind::Interface, None, json!({})),
                sol(
                    I_OCL_CALLER,
                    NodeKind::Field,
                    Some(I_OCL),
                    json!({"error": true, "params": []}),
                ),
                sol(
                    I_OCL_NOT_INIT,
                    NodeKind::Field,
                    Some(I_OCL),
                    json!({"error": true, "params": []}),
                ),
            ],
        );
        let imod = file(
            "src/interfaces/IERC7579Modules.sol",
            "",
            vec![
                sol(
                    "solidity:src/interfaces/IERC7579Modules.sol:interface:IModule",
                    NodeKind::Interface,
                    None,
                    json!({}),
                ),
                sol(
                    I_MOD_NOT_INIT,
                    NodeKind::Field,
                    Some("solidity:src/interfaces/IERC7579Modules.sol:interface:IModule"),
                    json!({"error": true, "params": [{"name": "smartAccount", "type": "address"}]}),
                ),
            ],
        );
        let packed = file(
            "src/interfaces/PackedUserOperation.sol",
            "",
            vec![sol(
                "solidity:src/interfaces/PackedUserOperation.sol:type:PackedUserOperation",
                NodeKind::Type,
                None,
                json!({"struct": true, "members": [
                    {"name": "sender", "type": "address"},
                    {"name": "nonce", "type": "uint256"},
                    {"name": "initCode", "type": "bytes"},
                    {"name": "callData", "type": "bytes"},
                    {"name": "accountGasLimits", "type": "bytes32"},
                    {"name": "preVerificationGas", "type": "uint256"},
                    {"name": "gasFees", "type": "bytes32"},
                    {"name": "paymasterAndData", "type": "bytes"},
                    {"name": "signature", "type": "bytes"}]}),
            )],
        );
        let types = file(
            "src/types/Types.sol",
            "",
            vec![
                sol(
                    "solidity:src/types/Types.sol:type_alias:CallType",
                    NodeKind::TypeAlias,
                    None,
                    json!({"udvt": true, "underlying": "bytes1"}),
                ),
                sol(
                    "solidity:src/types/Types.sol:enum:CallerPolicy",
                    NodeKind::Enum,
                    None,
                    json!({}),
                ),
                sol(
                    TYPES_ZERO,
                    NodeKind::Field,
                    None,
                    json!({"error": true, "params": []}),
                ),
            ],
        );
        let errors = file(
            "src/libraries/Errors.sol",
            "",
            vec![
                sol(ERRORS, NodeKind::Type, None, json!({"library": true})),
                sol(
                    ERRORS_ZERO,
                    NodeKind::Field,
                    Some(ERRORS),
                    json!({"error": true, "params": []}),
                ),
                sol(
                    ERRORS_CHECK,
                    NodeKind::Method,
                    Some(ERRORS),
                    json!({"visibility": "external", "params": [p("x", "uint256")]}),
                ),
            ],
        );
        let mut ocl = file(
            "src/OnChainLab.sol",
            "",
            vec![
                sol(OCL, NodeKind::Type, None, json!({"bases": ["IOnChainLab"]})),
                sol(
                    OCL_GUARD,
                    NodeKind::Function,
                    Some(OCL),
                    json!({"modifier": true, "params": []}),
                ),
                sol(
                    OCL_VALIDATE,
                    NodeKind::Method,
                    Some(OCL),
                    json!({"visibility": "external", "params": [
                        p("userOp", "PackedUserOperation"), p("userOpHash", "bytes32"),
                        p("missingAccountFunds", "uint256")],
                        "returns": [{"name": "validationData", "type": "uint256"}]}),
                ),
                sol(
                    OCL_POLICY,
                    NodeKind::Method,
                    Some(OCL),
                    json!({"visibility": "external", "params": [
                        p("callType", "CallType"), p("policy", "CallerPolicy"),
                        p("nft", "ILabNFT"), p("pair", "uint[2]")]}),
                ),
                sol(
                    OCL_EXECUTE,
                    NodeKind::Method,
                    Some(OCL),
                    json!({"visibility": "external", "params": [p("target", "IUnknownTarget")]}),
                ),
                sol(
                    OCL_INTERNAL,
                    NodeKind::Method,
                    Some(OCL),
                    json!({"visibility": "internal", "params": [p("callType", "CallType")]}),
                ),
                sol(
                    OCL_WITHDRAW,
                    NodeKind::Method,
                    Some(OCL),
                    json!({"visibility": "external", "params": []}),
                ),
            ],
        );
        ocl.1.imports = vec![
            imp("./interfaces/IOnChainLab.sol", &["IOnChainLab"]),
            imp("./interfaces/IERC7579Modules.sol", &["IModule"]),
            imp(
                "./interfaces/PackedUserOperation.sol",
                &["PackedUserOperation"],
            ),
            imp("./types/Types.sol", &[]),
            imp("./libraries/Errors.sol", &["Errors"]),
            imp("src/interfaces/ILabNFT.sol", &["ILabNFT"]),
        ];
        ocl.1.relations = vec![
            rel(OCL, EdgeKind::Extends, "IOnChainLab"),
            rel(OCL_GUARD, EdgeKind::Reverts, "NotInitialized"),
            rel(OCL_VALIDATE, EdgeKind::Reverts, "InvalidCaller"),
            rel(OCL_WITHDRAW, EdgeKind::Reverts, "Errors.Zero"),
        ];
        ocl.1.calls = vec![call(OCL_VALIDATE, "whenInitialized", None)];

        let mut tok = file(
            "src/OclTokenizer.sol",
            "",
            vec![
                sol(
                    TOK_MUST,
                    NodeKind::Field,
                    None,
                    json!({"error": true, "params": []}),
                ),
                sol(
                    "solidity:src/OclTokenizer.sol:type:OclTokenizer",
                    NodeKind::Type,
                    None,
                    json!({}),
                ),
                sol(
                    TOK_TOKENIZE,
                    NodeKind::Method,
                    Some("solidity:src/OclTokenizer.sol:type:OclTokenizer"),
                    json!({"visibility": "external", "params": [p("labId", "uint256")]}),
                ),
            ],
        );
        tok.1.relations = vec![rel(TOK_TOKENIZE, EdgeKind::Reverts, "MustControlLab")];

        let mut fbase = file(
            "src/factory/FactoryBase.sol",
            "",
            vec![
                sol(
                    FACTORY_BASE,
                    NodeKind::Type,
                    None,
                    json!({"abstract": true}),
                ),
                sol(
                    "solidity:src/factory/FactoryBase.sol:field:FactoryBase::labNft",
                    NodeKind::Field,
                    Some(FACTORY_BASE),
                    json!({"state_var": true, "visibility": "internal", "type": "ILabNFT"}),
                ),
            ],
        );
        fbase.1.imports = vec![imp("src/interfaces/ILabNFT.sol", &["ILabNFT"])];
        let mut factory = file(
            "src/factory/OnChainLabFactory.sol",
            "",
            vec![
                sol(
                    FACTORY,
                    NodeKind::Type,
                    None,
                    json!({"bases": ["FactoryBase"]}),
                ),
                sol(
                    FACTORY_CREATE,
                    NodeKind::Method,
                    Some(FACTORY),
                    json!({"visibility": "external", "params": [p("tokenId", "uint256")]}),
                ),
                sol(
                    FACTORY_NOTIFY,
                    NodeKind::Method,
                    Some(FACTORY),
                    json!({"visibility": "internal", "params": [p("tokenId", "uint256")]}),
                ),
                sol(
                    FACTORY_REGISTER,
                    NodeKind::Method,
                    Some(FACTORY),
                    json!({"visibility": "internal", "params": [p("tokenId", "uint256")]}),
                ),
            ],
        );
        factory.1.imports = vec![
            imp("src/interfaces/ILabNFT.sol", &["ILabNFT"]),
            imp("./FactoryBase.sol", &["FactoryBase"]),
        ];
        factory.1.relations = vec![rel(FACTORY, EdgeKind::Extends, "FactoryBase")];
        factory.1.calls = vec![
            // OnChainLabFactory.sol:276 `ILabNFT(labNftContract).notifyIdentityCreated(tokenId);`
            call(FACTORY_CREATE, "notifyIdentityCreated", Some("ILabNFT")),
            // `labNft` is a state variable of the base contract.
            call(FACTORY_NOTIFY, "notifyIdentityCreated", Some("labNft")),
            // `registry` is untyped: its `register` is not ours.
            call(FACTORY_CREATE, "register", Some("registry")),
        ];
        // A test mock with the same interface name: only imports tell them
        // apart, also when the importing file is a known (unchanged) one.
        let mock = file(
            "test/mocks/ILabNFT.sol",
            "",
            vec![
                sol(
                    "solidity:test/mocks/ILabNFT.sol:interface:ILabNFT",
                    NodeKind::Interface,
                    None,
                    json!({}),
                ),
                sol(
                    "solidity:test/mocks/ILabNFT.sol:method:ILabNFT::notifyIdentityCreated",
                    NodeKind::Method,
                    Some("solidity:test/mocks/ILabNFT.sol:interface:ILabNFT"),
                    json!({"visibility": "external", "params": [p("tokenId", "uint256")]}),
                ),
            ],
        );
        vec![
            ilab, base, lab, iocl, imod, packed, types, errors, ocl, tok, fbase, factory, mock,
        ]
    }

    fn sol_analyzed() -> Analyzed {
        analyze(repo(), &sol_fixture())
    }

    fn meta<'a>(a: &'a Analyzed, stable: &str) -> &'a Value {
        &node_by_stable(a, stable).metadata
    }

    #[test]
    fn solidity_root_relative_imports_resolve() {
        let mut a = file("contracts/src/factory/OnChainLabFactory.sol", "", vec![]);
        a.1.imports = vec![
            imp("src/interfaces/ILabNFT.sol", &["ILabNFT"]),
            imp("forge-std/Test.sol", &["Test"]),
        ];
        let files = vec![
            a,
            file("contracts/src/interfaces/ILabNFT.sol", "", vec![]),
            file("contracts/lib/forge-std/src/Test.sol", "", vec![]),
            file("src/interfaces/ILabNFT.sol", "", vec![]),
        ];
        let layout = SolidityLayout {
            projects: vec![FoundryProject {
                dir: "contracts".into(),
                remappings: vec![Remapping {
                    context: String::new(),
                    prefix: "forge-std/".into(),
                    target: "lib/forge-std/src/".into(),
                }],
                libs: vec!["lib".into()],
                src: "src".into(),
            }],
        };
        let r = analyze_with(repo(), &files, None, &layout);
        let from = "file:contracts/src/factory/OnChainLabFactory.sol";
        // The project dir is the base path, not the repository root.
        assert_eq!(
            edge_between(
                &r,
                from,
                "file:contracts/src/interfaces/ILabNFT.sol",
                EdgeKind::Imports
            ),
            Some(1.0)
        );
        assert!(edge_between(
            &r,
            from,
            "file:src/interfaces/ILabNFT.sol",
            EdgeKind::Imports
        )
        .is_none());
        assert_eq!(
            edge_between(
                &r,
                from,
                "file:contracts/lib/forge-std/src/Test.sol",
                EdgeKind::Imports
            ),
            Some(1.0)
        );
        assert_eq!(r.stats.imports_resolved, 2);
        // Without a Foundry project the repository root is the base path.
        let r = analyze(repo(), &files);
        assert_eq!(
            edge_between(
                &r,
                from,
                "file:src/interfaces/ILabNFT.sol",
                EdgeKind::Imports
            ),
            Some(1.0)
        );
        assert_eq!(r.stats.imports_resolved, 1);
        let mut b = file("src/factory/OnChainLabFactory.sol", "", vec![]);
        b.1.imports = vec![imp("src/interfaces/ILabNFT.sol", &["ILabNFT"])];
        let r = analyze(repo(), &[b, file("src/interfaces/ILabNFT.sol", "", vec![])]);
        assert!(edge_between(
            &r,
            "file:src/factory/OnChainLabFactory.sol",
            "file:src/interfaces/ILabNFT.sol",
            EdgeKind::Imports
        )
        .is_some());
    }

    #[test]
    fn solidity_remapped_import_records_remap_key_as_package() {
        // labs-contracts remappings.txt: the longest prefix wins.
        let layout = SolidityLayout {
            projects: vec![FoundryProject {
                dir: String::new(),
                remappings: crate::domain::solidity::parse_remappings(
                    "@openzeppelin/contracts-upgradeable/=dependencies/@openzeppelin-contracts-upgradeable-5.5.0/\n@openzeppelin/contracts/=dependencies/@openzeppelin-contracts-5.5.0/\nforge-std/=dependencies/forge-std-1.12.0/src/\n",
                ),
                libs: vec!["dependencies".into()],
                src: "src".into(),
            }],
        };
        let mut a = file("src/AccessResolver.sol", "", vec![]);
        a.1.imports = vec![
            imp(
                "@openzeppelin/contracts-upgradeable/proxy/utils/Initializable.sol",
                &["Initializable"],
            ),
            imp("@openzeppelin/contracts/utils/Strings.sol", &["Strings"]),
            imp("solady/utils/LibString.sol", &["LibString"]),
            imp("@scope/pkg/X.sol", &["X"]),
            imp("./IOcl.sol", &["IOcl"]),
        ];
        let r = analyze_with(repo(), &[a], None, &layout);
        assert_eq!(
            meta(&r, "file:src/AccessResolver.sol")["packages"],
            json!([
                "@openzeppelin/contracts-upgradeable",
                "@openzeppelin/contracts",
                "solady",
                "@scope/pkg"
            ])
        );
    }

    #[test]
    fn solidity_remap_to_repo_path_resolves() {
        let layout = SolidityLayout {
            projects: vec![FoundryProject {
                dir: String::new(),
                remappings: crate::domain::solidity::parse_remappings("@ocl/=src/\n"),
                libs: vec!["dependencies".into()],
                src: "src".into(),
            }],
        };
        let mut a = file("test/LabNFT.t.sol", "", vec![]);
        a.1.imports = vec![imp("@ocl/interfaces/ILabNFT.sol", &["ILabNFT"])];
        let files = vec![a, file("src/interfaces/ILabNFT.sol", "", vec![])];
        let r = analyze_with(repo(), &files, None, &layout);
        assert!(edge_between(
            &r,
            "file:test/LabNFT.t.sol",
            "file:src/interfaces/ILabNFT.sol",
            EdgeKind::Imports
        )
        .is_some());
        assert_eq!(meta(&r, "file:test/LabNFT.t.sol")["packages"], json!([]));
    }

    #[test]
    fn solidity_whole_file_import_uses_top_level_names() {
        // onchainlabs: `import "./types/Types.sol";` brings CallType and
        // CallerPolicy in; a body naming them lists them as uses.
        let src = "function f(CallType c, CallerPolicy p) external {}\n";
        let mut a = file(
            "src/OnChainLab.sol",
            src,
            vec![Symbol {
                byte_start: 0,
                byte_end: src.len(),
                ..sol(
                    "solidity:src/OnChainLab.sol:function:f",
                    NodeKind::Function,
                    None,
                    json!({}),
                )
            }],
        );
        a.1.imports = vec![imp("./types/Types.sol", &[])];
        let files: Vec<_> = std::iter::once(a)
            .chain(
                sol_fixture()
                    .into_iter()
                    .filter(|f| f.0.path == "src/types/Types.sol"),
            )
            .collect();
        let r = analyze(repo(), &files);
        let u = r.units.iter().find(|u| u.symbol.name == "f").unwrap();
        assert_eq!(u.uses, vec!["CallType", "CallerPolicy"]);
    }

    #[test]
    fn solidity_extends_confidence_rises_with_imports() {
        let fx = sol_fixture();
        let mut files: Vec<_> = fx
            .into_iter()
            .filter(|f| {
                matches!(
                    f.0.path.as_str(),
                    "src/NFT/LabNFT.sol" | "src/interfaces/ILabNFT.sol" | "src/NFT/ERC721Base.sol"
                )
            })
            .collect();
        // A second ILabNFT (a test mock): only the import disambiguates.
        files.push(file(
            "test/mocks/ILabNFT.sol",
            "",
            vec![sol(
                "solidity:test/mocks/ILabNFT.sol:interface:ILabNFT",
                NodeKind::Interface,
                None,
                json!({}),
            )],
        ));
        let r = analyze(repo(), &files);
        assert_eq!(
            edge_between(&r, LAB, I_LAB, EdgeKind::Extends),
            Some(CONF_IMPORTED)
        );
        let mut no_import = files.clone();
        no_import[0].1.imports.clear();
        let lab_pos = no_import
            .iter()
            .position(|f| f.0.path == "src/NFT/LabNFT.sol")
            .unwrap();
        no_import[lab_pos].1.imports.clear();
        let r = analyze(repo(), &no_import);
        assert_eq!(edge_between(&r, LAB, I_LAB, EdgeKind::Extends), None);
        assert_eq!(
            edge_between(&r, LAB, BASE, EdgeKind::Extends),
            Some(CONF_REPO_UNIQUE)
        );
    }

    #[test]
    fn solidity_emit_resolves_to_interface_event() {
        // LabNFT.sol:94 `emit Mint(to, tokenId);` → ILabNFT.sol:14.
        let a = sol_analyzed();
        assert_eq!(
            edge_between(&a, LAB_MINT, I_LAB_MINT, EdgeKind::Emits),
            Some(CONF_IMPORTED)
        );
    }

    #[test]
    fn solidity_revert_prefers_ancestor_error() {
        // `modifier whenInitialized() { … revert NotInitialized(); }`: declared
        // in IOnChainLab (an ancestor) and IModule (imported, unrelated).
        let a = sol_analyzed();
        assert_eq!(
            edge_between(&a, OCL_GUARD, I_OCL_NOT_INIT, EdgeKind::Reverts),
            Some(CONF_IMPORTED)
        );
        assert_eq!(
            edge_between(&a, OCL_GUARD, I_MOD_NOT_INIT, EdgeKind::Reverts),
            None
        );
        // Without the `is IOnChainLab` base the one remaining declaration is
        // another contract's member, out of scope: no edge.
        let mut fx = sol_fixture();
        fx.retain(|f| f.0.path != "src/interfaces/IERC7579Modules.sol");
        let ocl = fx
            .iter_mut()
            .find(|f| f.0.path == "src/OnChainLab.sol")
            .unwrap();
        ocl.1.symbols[0].metadata = json!({});
        ocl.1.relations.retain(|r| r.kind != EdgeKind::Extends);
        let b = analyze(repo(), &fx);
        let guard = node_by_stable(&b, OCL_GUARD).id;
        assert!(!b
            .bundle
            .edges
            .iter()
            .any(|e| e.kind == EdgeKind::Reverts && e.source_node_id == guard));
        assert_eq!(
            edge_between(&a, OCL_VALIDATE, I_OCL_CALLER, EdgeKind::Reverts),
            Some(CONF_IMPORTED)
        );
    }

    #[test]
    fn solidity_qualified_revert() {
        let a = sol_analyzed();
        assert_eq!(
            edge_between(&a, OCL_WITHDRAW, ERRORS_ZERO, EdgeKind::Reverts),
            Some(CONF_IMPORTED)
        );
        assert_eq!(
            edge_between(&a, OCL_WITHDRAW, TYPES_ZERO, EdgeKind::Reverts),
            None
        );
    }

    #[test]
    fn solidity_file_level_error_same_file() {
        // ocltokenizer OclTokenizer.sol:16 `error MustControlLab();`.
        let a = sol_analyzed();
        assert_eq!(
            edge_between(&a, TOK_TOKENIZE, TOK_MUST, EdgeKind::Reverts),
            Some(CONF_SAME_FILE)
        );
    }

    #[test]
    fn solidity_interface_cast_call_binds_method() {
        let a = sol_analyzed();
        assert_eq!(
            edge_between(&a, FACTORY_CREATE, I_LAB_NOTIFY, EdgeKind::Calls),
            Some(CONF_RECEIVER)
        );
        // The modifier invocation binds the contract's own modifier.
        assert_eq!(
            edge_between(&a, OCL_VALIDATE, OCL_GUARD, EdgeKind::Calls),
            Some(CONF_SAME_FILE)
        );
    }

    #[test]
    fn solidity_untyped_receiver_reaches_free_functions_only() {
        // `registry.register(id)` with `registry` of unknown type never binds
        // the caller's own `register`.
        let a = sol_analyzed();
        assert_eq!(
            edge_between(&a, FACTORY_CREATE, FACTORY_REGISTER, EdgeKind::Calls),
            None
        );
        // A free function attached with `using {f} for T` still binds.
        let mut f = file(
            "src/types/Types.sol",
            "",
            vec![
                sol(
                    "solidity:src/types/Types.sol:function:vTypeEqual",
                    NodeKind::Function,
                    None,
                    json!({}),
                ),
                sol(
                    "solidity:src/types/Types.sol:type:Lab",
                    NodeKind::Type,
                    None,
                    json!({}),
                ),
                sol(
                    "solidity:src/types/Types.sol:method:Lab::vTypeEqual",
                    NodeKind::Method,
                    Some("solidity:src/types/Types.sol:type:Lab"),
                    json!({}),
                ),
                sol(
                    "solidity:src/types/Types.sol:method:Lab::check",
                    NodeKind::Method,
                    Some("solidity:src/types/Types.sol:type:Lab"),
                    json!({}),
                ),
            ],
        );
        f.1.calls = vec![call(
            "solidity:src/types/Types.sol:method:Lab::check",
            "vTypeEqual",
            Some("a"),
        )];
        let r = analyze(repo(), &[f]);
        assert_eq!(
            edge_between(
                &r,
                "solidity:src/types/Types.sol:method:Lab::check",
                "solidity:src/types/Types.sol:function:vTypeEqual",
                EdgeKind::Calls
            ),
            Some(CONF_SAME_FILE)
        );
        assert_eq!(r.stats.calls_resolved, 1);
    }

    #[test]
    fn solidity_unqualified_call_never_binds_foreign_member() {
        // labs-contracts AccessResolver: `owner()` comes from the unindexed
        // OwnableUpgradeable base, not from the imported `IOcl`.
        const AR: &str = "solidity:src/AccessResolver.sol:type:AccessResolver";
        const AR_FN: &str = "solidity:src/AccessResolver.sol:method:AccessResolver::_isLabOwner";
        const HELPER: &str = "solidity:src/AccessResolver.sol:function:toKey";
        let mut ar = file(
            "src/AccessResolver.sol",
            "",
            vec![
                sol(
                    AR,
                    NodeKind::Type,
                    None,
                    json!({"bases": ["OwnableUpgradeable"]}),
                ),
                sol(
                    AR_FN,
                    NodeKind::Method,
                    Some(AR),
                    json!({"visibility": "internal"}),
                ),
                sol(HELPER, NodeKind::Function, None, json!({})),
            ],
        );
        ar.1.imports = vec![imp("./interfaces/IOcl.sol", &["IOcl"])];
        ar.1.relations = vec![rel(AR, EdgeKind::Extends, "OwnableUpgradeable")];
        ar.1.calls = vec![
            call(AR_FN, "owner", None),
            call(AR_FN, "toKey", None),
            call(AR_FN, "IOcl", None),
            call(AR_FN, "owner", Some("this")),
        ];
        let iocl = file(
            "src/interfaces/IOcl.sol",
            "",
            vec![
                sol(
                    "solidity:src/interfaces/IOcl.sol:interface:IOcl",
                    NodeKind::Interface,
                    None,
                    json!({}),
                ),
                sol(
                    "solidity:src/interfaces/IOcl.sol:method:IOcl::owner",
                    NodeKind::Method,
                    Some("solidity:src/interfaces/IOcl.sol:interface:IOcl"),
                    json!({"visibility": "external"}),
                ),
            ],
        );
        let a = analyze(repo(), &[ar, iocl]);
        assert_eq!(
            edge_between(
                &a,
                AR_FN,
                "solidity:src/interfaces/IOcl.sol:method:IOcl::owner",
                EdgeKind::Calls
            ),
            None
        );
        assert_eq!(
            edge_between(&a, AR_FN, HELPER, EdgeKind::Calls),
            Some(CONF_SAME_FILE)
        );
        // The cast `IOcl(tba)` binds the interface itself.
        assert_eq!(
            edge_between(
                &a,
                AR_FN,
                "solidity:src/interfaces/IOcl.sol:interface:IOcl",
                EdgeKind::Calls
            ),
            Some(CONF_IMPORTED)
        );
        assert_eq!(a.stats.calls_resolved, 2);
    }

    #[test]
    fn solidity_super_call_binds_ancestor_method() {
        // LabNFT.sol:220 `super._beforeTokenTransfer(from, to, tokenId);`
        let a = sol_analyzed();
        assert_eq!(
            edge_between(&a, LAB_HOOK, BASE_HOOK, EdgeKind::Calls),
            Some(CONF_RECEIVER)
        );
    }

    #[test]
    fn solidity_inherited_state_var_types_receiver() {
        let a = sol_analyzed();
        assert_eq!(
            edge_between(&a, FACTORY_NOTIFY, I_LAB_NOTIFY, EdgeKind::Calls),
            Some(CONF_RECEIVER)
        );
    }

    #[test]
    fn solidity_override_implements_interface_method() {
        let a = sol_analyzed();
        assert_eq!(
            edge_between(&a, LAB_NOTIFY, I_LAB_NOTIFY, EdgeKind::Implements),
            Some(CONF_IMPORTED)
        );
        assert_eq!(
            edge_between(&a, LAB_HOOK, BASE_HOOK, EdgeKind::Implements),
            Some(CONF_IMPORTED)
        );
        // `uint256 public mintFeeWei;` implements `function mintFeeWei() external view`.
        assert_eq!(
            edge_between(&a, LAB_FEE, I_LAB_FEE, EdgeKind::Implements),
            Some(CONF_IMPORTED)
        );
        // `mint` has no counterpart in a base.
        assert!(
            !a.bundle.edges.iter().any(|e| e.kind == EdgeKind::Implements
                && e.source_node_id == node_by_stable(&a, LAB_MINT).id)
        );
    }

    #[test]
    fn solidity_incremental_matches_full() {
        let fx = sol_fixture();
        let full = analyze(repo(), &fx);
        let kinds = [
            EdgeKind::Emits,
            EdgeKind::Reverts,
            EdgeKind::Implements,
            EdgeKind::Calls,
            EdgeKind::Extends,
        ];
        for kind in kinds {
            assert!(full.bundle.edges.iter().any(|e| e.kind == kind), "{kind:?}");
        }
        for (i, changed) in fx.iter().enumerate() {
            let fid = full.bundle.files[i].id;
            let ids: HashSet<Uuid> = full
                .bundle
                .nodes
                .iter()
                .filter(|n| n.file_id == Some(fid))
                .map(|n| n.id)
                .collect();
            let known = Graph {
                files: full
                    .bundle
                    .files
                    .iter()
                    .filter(|f| f.id != fid)
                    .cloned()
                    .collect(),
                nodes: full
                    .bundle
                    .nodes
                    .iter()
                    .filter(|n| !ids.contains(&n.id))
                    .cloned()
                    .collect(),
                edges: full
                    .bundle
                    .edges
                    .iter()
                    .filter(|e| !ids.contains(&e.source_node_id))
                    .cloned()
                    .collect(),
            };
            let inc = analyze_with(
                repo(),
                std::slice::from_ref(changed),
                Some(&known),
                &SolidityLayout::default(),
            );
            let out_edges = |a: &Analyzed| {
                let mut v: Vec<(Uuid, Uuid, &'static str, u32)> = a
                    .bundle
                    .edges
                    .iter()
                    .filter(|e| ids.contains(&e.source_node_id))
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
            assert_eq!(out_edges(&inc), out_edges(&full), "{}", changed.0.path);
            let metas = |a: &Analyzed| {
                let mut v: Vec<(String, Value)> = a
                    .bundle
                    .nodes
                    .iter()
                    .filter(|n| ids.contains(&n.id))
                    .map(|n| (n.stable_id.clone(), n.metadata.clone()))
                    .collect();
                v.sort_by(|x, y| x.0.cmp(&y.0));
                v
            };
            assert_eq!(metas(&inc), metas(&full), "{}", changed.0.path);
            let abis = |a: &Analyzed, fi: usize| {
                a.units
                    .iter()
                    .filter(|u| u.file_index == fi)
                    .map(|u| (u.symbol.stable_id.clone(), u.abi.clone()))
                    .collect::<Vec<_>>()
            };
            assert_eq!(abis(&inc, 0), abis(&full, i), "{}", changed.0.path);
        }
    }

    #[test]
    fn abi_struct_param_is_tuple() {
        // ERC-4337 v0.7 `validateUserOp(PackedUserOperation,bytes32,uint256)`.
        let a = sol_analyzed();
        let m = meta(&a, OCL_VALIDATE);
        let sig = "validateUserOp((address,uint256,bytes,bytes,bytes32,uint256,bytes32,bytes,bytes),bytes32,uint256)";
        assert_eq!(m["abi_signature"], sig);
        assert_eq!(m["selector"], "0x19822f7c");
        let u = a
            .units
            .iter()
            .find(|u| u.symbol.stable_id == OCL_VALIDATE)
            .unwrap();
        assert_eq!(u.abi.as_deref(), Some(format!("{sig} 0x19822f7c").as_str()));
    }

    #[test]
    fn abi_udvt_enum_interface_canonicalize() {
        let a = sol_analyzed();
        let m = meta(&a, OCL_POLICY);
        assert_eq!(
            m["abi_signature"],
            "setPolicy(bytes1,uint8,address,uint256[2])"
        );
        assert_eq!(
            m["selector"],
            hash::selector_hex("setPolicy(bytes1,uint8,address,uint256[2])")
        );
        // Events get topic0, errors a selector.
        assert_eq!(
            meta(&a, I_LAB_MINT)["abi_signature"],
            "Mint(address,uint256)"
        );
        assert_eq!(
            meta(&a, I_LAB_MINT)["topic0"],
            hash::topic_hex("Mint(address,uint256)")
        );
        assert!(meta(&a, I_LAB_MINT).get("selector").is_none());
        assert_eq!(
            meta(&a, I_MOD_NOT_INIT)["abi_signature"],
            "NotInitialized(address)"
        );
        assert_eq!(
            meta(&a, I_MOD_NOT_INIT)["selector"],
            hash::selector_hex("NotInitialized(address)")
        );
    }

    #[test]
    fn abi_public_mapping_getter() {
        let a = sol_analyzed();
        assert_eq!(
            meta(&a, LAB_DID)["abi_signature"],
            "activeDidHash(bytes32,bytes32,bytes32)"
        );
        assert_eq!(
            meta(&a, LAB_DID)["selector"],
            hash::selector_hex("activeDidHash(bytes32,bytes32,bytes32)")
        );
        // LabNFT.sol:46 `uint256 public mintFeeWei;`, read by desci-ecosystem.
        assert_eq!(meta(&a, LAB_FEE)["abi_signature"], "mintFeeWei()");
        assert_eq!(
            meta(&a, LAB_FEE)["selector"],
            hash::selector_hex("mintFeeWei()")
        );
    }

    #[test]
    fn abi_unresolved_type_yields_no_selector() {
        let a = sol_analyzed();
        let m = meta(&a, OCL_EXECUTE);
        assert_eq!(m["abi_unresolved"], json!(["IUnknownTarget"]));
        assert!(m.get("abi_signature").is_none());
        assert!(m.get("selector").is_none());
        let u = a
            .units
            .iter()
            .find(|u| u.symbol.stable_id == OCL_EXECUTE)
            .unwrap();
        assert_eq!(u.abi, None);
        // A non-literal array length is never guessed either.
        let none = HashMap::new();
        let s = Sol::new(&[], &none, &HashMap::new(), &[]);
        let mut w = AbiWalk::default();
        assert_eq!(s.abi_type(0, None, "uint256[MAX]", 0, &mut w), None);
        assert_eq!(w.unresolved, vec!["uint256[MAX]"]);
        assert_eq!(
            s.abi_type(0, None, "address payable[][3]", 0, &mut w)
                .as_deref(),
            Some("address[][3]")
        );
        assert_eq!(
            s.abi_type(
                0,
                None,
                "function (uint) external returns (bool)",
                0,
                &mut w
            )
            .as_deref(),
            Some("function")
        );
    }

    #[test]
    fn abi_skips_library_and_internal() {
        let a = sol_analyzed();
        for id in [OCL_INTERNAL, ERRORS_CHECK, OCL_GUARD, LAB_HOOK] {
            let m = meta(&a, id);
            assert!(
                m.get("abi_signature").is_none() && m.get("abi_unresolved").is_none(),
                "{id}"
            );
        }
        // A library's custom error still has a selector.
        assert_eq!(
            meta(&a, ERRORS_ZERO)["selector"],
            hash::selector_hex("Zero()")
        );
        // The interface method itself is in the ABI.
        assert_eq!(
            meta(&a, I_LAB_NOTIFY)["abi_signature"],
            "notifyIdentityCreated(uint256)"
        );
    }

    /// `contract Base { function f(<base>) … }` and `contract Child is Base
    /// { function f(<child>) … }` in one file, analyzed together.
    fn override_pair(base_params: Value, child_params: Value, visibility: &str) -> Analyzed {
        const B: &str = "solidity:src/X.sol:type:Base";
        const C: &str = "solidity:src/X.sol:type:Child";
        let mut x = file(
            "src/X.sol",
            "",
            vec![
                sol(B, NodeKind::Type, None, json!({})),
                sol(
                    "solidity:src/X.sol:method:Base::f",
                    NodeKind::Method,
                    Some(B),
                    json!({"visibility": visibility, "virtual": true, "params": base_params}),
                ),
                sol(C, NodeKind::Type, None, json!({"bases": ["Base"]})),
                sol(
                    "solidity:src/X.sol:method:Child::f",
                    NodeKind::Method,
                    Some(C),
                    json!({"visibility": visibility, "params": child_params}),
                ),
            ],
        );
        x.1.relations = vec![rel(C, EdgeKind::Extends, "Base")];
        analyze(repo(), &[x])
    }

    #[test]
    fn solidity_override_needs_same_parameter_types() {
        let child_f = "solidity:src/X.sol:method:Child::f";
        let base_f = "solidity:src/X.sol:method:Base::f";
        // A same-arity overload is not an override (review finding: it got
        // an `Implements` edge at 1.0).
        for vis in ["public", "internal"] {
            let a = override_pair(json!([p("x", "uint256")]), json!([p("a", "address")]), vis);
            assert_eq!(
                edge_between(&a, child_f, base_f, EdgeKind::Implements),
                None,
                "{vis}"
            );
        }
        // `uint` and `uint256` are one type, in and out of the ABI.
        for vis in ["public", "internal"] {
            let a = override_pair(json!([p("x", "uint256")]), json!([p("y", "uint")]), vis);
            assert_eq!(
                edge_between(&a, child_f, base_f, EdgeKind::Implements),
                Some(CONF_SAME_FILE),
                "{vis}"
            );
        }
        // `address payable` is its own type outside the ABI.
        let a = override_pair(
            json!([p("x", "address")]),
            json!([p("y", "address payable")]),
            "internal",
        );
        assert_eq!(
            edge_between(&a, child_f, base_f, EdgeKind::Implements),
            None
        );
    }

    #[test]
    fn solidity_override_picks_the_matching_overload() {
        const B: &str = "solidity:src/X.sol:type:Base";
        const C: &str = "solidity:src/X.sol:type:Child";
        let f_uint = "solidity:src/X.sol:method:Base::f";
        let f_addr = "solidity:src/X.sol:method:Base::f#2";
        let child_f = "solidity:src/X.sol:method:Child::f";
        let mut overload = sol(
            f_addr,
            NodeKind::Method,
            Some(B),
            json!({"visibility": "public", "virtual": true, "params": [p("a", "address")]}),
        );
        overload.name = "f".into();
        let mut x = file(
            "src/X.sol",
            "",
            vec![
                sol(B, NodeKind::Type, None, json!({})),
                sol(
                    f_uint,
                    NodeKind::Method,
                    Some(B),
                    json!({"visibility": "public", "virtual": true, "params": [p("x", "uint256")]}),
                ),
                overload,
                sol(C, NodeKind::Type, None, json!({"bases": ["Base"]})),
                sol(
                    child_f,
                    NodeKind::Method,
                    Some(C),
                    json!({"visibility": "public", "override": true, "params": [p("a", "address")]}),
                ),
            ],
        );
        x.1.relations = vec![rel(C, EdgeKind::Extends, "Base")];
        let a = analyze(repo(), &[x]);
        assert_eq!(
            edge_between(&a, child_f, f_addr, EdgeKind::Implements),
            Some(CONF_SAME_FILE)
        );
        assert_eq!(
            edge_between(&a, child_f, f_uint, EdgeKind::Implements),
            None
        );
    }

    #[test]
    fn abi_recursive_struct_is_unresolved_without_blowup() {
        // `struct Node { uint256 v; Node[] c0; … Node[] c11; }` used by an
        // event: before the visiting set, this expanded width^8 times.
        const S: &str = "solidity:src/R.sol:type:Node";
        const EV: &str = "solidity:src/R.sol:field:A::E";
        const A: &str = "solidity:src/R.sol:type:A";
        let mut members = vec![p("v", "uint256")];
        members.extend((0..12).map(|i| p(&format!("c{i}"), "Node[]")));
        // Mutual recursion: `struct Ping { Pong p; } struct Pong { Ping[] q; }`.
        const PING: &str = "solidity:src/R.sol:type:Ping";
        const PONG: &str = "solidity:src/R.sol:type:Pong";
        const ERR: &str = "solidity:src/R.sol:field:A::Bad";
        let x = file(
            "src/R.sol",
            "",
            vec![
                sol(
                    S,
                    NodeKind::Type,
                    None,
                    json!({"struct": true, "members": members}),
                ),
                sol(
                    PING,
                    NodeKind::Type,
                    None,
                    json!({"struct": true, "members": [p("p", "Pong")]}),
                ),
                sol(
                    PONG,
                    NodeKind::Type,
                    None,
                    json!({"struct": true, "members": [p("q", "Ping[]")]}),
                ),
                sol(A, NodeKind::Type, None, json!({})),
                sol(
                    EV,
                    NodeKind::Field,
                    Some(A),
                    json!({"event": true, "params": [p("n", "Node")]}),
                ),
                sol(
                    ERR,
                    NodeKind::Field,
                    Some(A),
                    json!({"error": true, "params": [p("x", "uint"), p("p", "Ping")]}),
                ),
            ],
        );
        let started = std::time::Instant::now();
        let a = analyze(repo(), &[x]);
        // Hours before the fix; microseconds after. Generous for a loaded box.
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert_eq!(meta(&a, EV)["abi_unresolved"], json!(["Node"]));
        assert!(meta(&a, EV).get("topic0").is_none());
        assert_eq!(meta(&a, ERR)["abi_unresolved"], json!(["Ping"]));
        assert!(meta(&a, ERR).get("selector").is_none());
    }

    #[test]
    fn abi_shared_struct_resolves_on_every_path() {
        // A struct reached several times, not recursively, expands every
        // time (the per-signature memo returns the same tuple).
        const P: &str = "solidity:src/S.sol:type:Pair";
        const W: &str = "solidity:src/S.sol:type:Wrap";
        const A: &str = "solidity:src/S.sol:type:A";
        const OK: &str = "solidity:src/S.sol:field:A::Ok";
        let x = file(
            "src/S.sol",
            "",
            vec![
                sol(
                    P,
                    NodeKind::Type,
                    None,
                    json!({"struct": true, "members": [p("a", "uint"), p("b", "bool")]}),
                ),
                sol(
                    W,
                    NodeKind::Type,
                    None,
                    json!({"struct": true, "members": [p("l", "Pair"), p("r", "Pair[2]")]}),
                ),
                sol(A, NodeKind::Type, None, json!({})),
                sol(
                    OK,
                    NodeKind::Field,
                    Some(A),
                    json!({"event": true, "params": [p("w", "Wrap"), p("p", "Pair")]}),
                ),
            ],
        );
        let a = analyze(repo(), &[x]);
        assert_eq!(
            meta(&a, OK)["abi_signature"],
            "Ok(((uint256,bool),(uint256,bool)[2]),(uint256,bool))"
        );
    }

    const RESOLVER: &str = "typescript:lambda/labs/index.ts:resolver:originalHandler::createLab";
    const MEMBERS: &str = "typescript:lambda/labs/index.ts:resolver:originalHandler::members";
    const LAB_MEMBERS: &str =
        "typescript:lambda/labs/index.ts:resolver:originalHandler::labMembers";
    const GET_RESOLVERS: &str = "typescript:lib/c.ts:method:Config::getResolvers";
    const MERGED: &str = "graphql:schemas/merged-schema.graphql:field:Mutation.createLab";

    /// The labs Lambda: a bare `createLab` case, an unguarded bare `members`
    /// case, and a guarded case resolving `Lab.members`.
    fn lambda() -> (FileInput, FileExtraction) {
        let host = "typescript:lambda/labs/index.ts:function:originalHandler";
        let mut f = file(
            "lambda/labs/index.ts",
            "export const originalHandler = async () => {}",
            vec![
                sym(host, NodeKind::Function, "originalHandler", None),
                sym(RESOLVER, NodeKind::Resolver, "createLab", Some(host)),
                sym(MEMBERS, NodeKind::Resolver, "members", Some(host)),
                sym(LAB_MEMBERS, NodeKind::Resolver, "labMembers", Some(host)),
            ],
        );
        f.1.relations = vec![
            rel(RESOLVER, EdgeKind::Resolves, "createLab"),
            rel(MEMBERS, EdgeKind::Resolves, "members"),
            rel(LAB_MEMBERS, EdgeKind::Resolves, "Lab.members"),
        ];
        f
    }

    fn config() -> (FileInput, FileExtraction) {
        let mut f = file(
            "lib/c.ts",
            "export class Config { getResolvers() { return []; } }",
            vec![
                sym(
                    "typescript:lib/c.ts:type:Config",
                    NodeKind::Type,
                    "Config",
                    None,
                ),
                sym(
                    GET_RESOLVERS,
                    NodeKind::Method,
                    "getResolvers",
                    Some("typescript:lib/c.ts:type:Config"),
                ),
            ],
        );
        f.1.relations = vec![rel(GET_RESOLVERS, EdgeKind::Binds, "Mutation.createLab")];
        f
    }

    fn schemas() -> Vec<(FileInput, FileExtraction)> {
        vec![
            sdl("schemas/ip-hubs.graphql", &["Mutation.createLab"]),
            sdl(
                "schemas/merged-schema.graphql",
                &[
                    "Mutation.createLab",
                    "Query.labs",
                    "Lab.members",
                    "LabRef.members",
                ],
            ),
        ]
    }

    #[test]
    fn queries_and_binds_land_on_the_canonical_sdl_copy() {
        let mut op = file(
            "src/lab.ts",
            "const Q = gql`mutation CreateLab { createLab { id } }`;",
            vec![sym(
                "graphql:src/lab.ts:operation:CreateLab",
                NodeKind::Operation,
                "CreateLab",
                None,
            )],
        );
        op.1.relations = vec![rel(
            "graphql:src/lab.ts:operation:CreateLab",
            EdgeKind::Queries,
            "Mutation.createLab",
        )];
        let mut files = schemas();
        files.push(op);
        files.push(config());
        let a = analyze(repo(), &files);
        // Two copies of Mutation.createLab: merged-schema (2 root fields) wins
        // over ip-hubs (1); today's `unique` dropped both edges.
        assert_eq!(
            edge_between(
                &a,
                "graphql:src/lab.ts:operation:CreateLab",
                MERGED,
                EdgeKind::Queries
            ),
            Some(1.0)
        );
        assert_eq!(
            edge_between(&a, GET_RESOLVERS, MERGED, EdgeKind::Binds),
            Some(1.0)
        );
        let ip_hubs = node_by_stable(
            &a,
            "graphql:schemas/ip-hubs.graphql:field:Mutation.createLab",
        )
        .id;
        assert!(!a
            .bundle
            .edges
            .iter()
            .any(|e| e.target_node_id == ip_hubs && e.kind != EdgeKind::Contains));
        // Tie on root fields: the smallest path is canonical.
        let tie = vec![
            sdl("b.graphql", &["Query.x"]),
            sdl("a.graphql", &["Query.x"]),
            {
                let mut c = config();
                c.1.relations = vec![rel(GET_RESOLVERS, EdgeKind::Binds, "Query.x")];
                c
            },
        ];
        let a = analyze(repo(), &tie);
        assert_eq!(
            edge_between(
                &a,
                GET_RESOLVERS,
                "graphql:a.graphql:field:Query.x",
                EdgeKind::Binds
            ),
            Some(1.0)
        );
    }

    #[test]
    fn resolves_uses_guards_bindings_then_root_uniqueness() {
        let mut files = schemas();
        files.push(lambda());
        files.push(config());
        let a = analyze(repo(), &files);
        // Bare `createLab`, bound by a registration → 0.9 on the canonical copy.
        assert_eq!(
            edge_between(&a, RESOLVER, MERGED, EdgeKind::Resolves),
            Some(CONF_RESOLVES_BOUND)
        );
        // Typed `Lab.members` (a parentTypeName guard) → 0.9.
        assert_eq!(
            edge_between(
                &a,
                LAB_MEMBERS,
                "graphql:schemas/merged-schema.graphql:field:Lab.members",
                EdgeKind::Resolves
            ),
            Some(CONF_RESOLVES_TYPED)
        );
        // Bare `members`: Lab.members and LabRef.members, neither root nor bound → none.
        let members = node_by_stable(&a, MEMBERS).id;
        assert!(!a
            .bundle
            .edges
            .iter()
            .any(|e| e.source_node_id == members && e.kind == EdgeKind::Resolves));
        // Without the registration the root field is still unique → 0.7.
        let mut files = schemas();
        files.push(lambda());
        let a = analyze(repo(), &files);
        assert_eq!(
            edge_between(&a, RESOLVER, MERGED, EdgeKind::Resolves),
            Some(CONF_RESOLVES_UNIQUE)
        );
    }

    /// desci-infra `case "legalAgreementStatus"`: guarded Lab / LabRef
    /// branches, then the Query path (a bare label next to the typed ones).
    #[test]
    fn bare_label_resolves_among_the_coordinates_its_guards_did_not_name() {
        const LEGAL: &str =
            "typescript:lambda/labs/index.ts:resolver:originalHandler::legalAgreementStatus";
        let host = "typescript:lambda/labs/index.ts:function:originalHandler";
        let mut lambda = file(
            "lambda/labs/index.ts",
            "export const originalHandler = async () => {}",
            vec![
                sym(host, NodeKind::Function, "originalHandler", None),
                sym(
                    LEGAL,
                    NodeKind::Resolver,
                    "legalAgreementStatus",
                    Some(host),
                ),
            ],
        );
        lambda.1.relations = vec![
            rel(LEGAL, EdgeKind::Resolves, "legalAgreementStatus"),
            rel(LEGAL, EdgeKind::Resolves, "Lab.legalAgreementStatus"),
            rel(LEGAL, EdgeKind::Resolves, "LabRef.legalAgreementStatus"),
        ];
        let coords = [
            "Query.legalAgreementStatus",
            "Lab.legalAgreementStatus",
            "LabRef.legalAgreementStatus",
        ];
        let mut cfg = config();
        cfg.1.relations = coords
            .iter()
            .map(|c| rel(GET_RESOLVERS, EdgeKind::Binds, c))
            .collect();
        let schema = sdl("schemas/merged-schema.graphql", &coords);
        let a = analyze(repo(), &[schema.clone(), lambda.clone(), cfg]);
        let at = |c: &str| format!("graphql:schemas/merged-schema.graphql:field:{c}");
        // All three are bound; minus the two guarded ones, Query is the one bound.
        for (c, conf) in [
            ("Query.legalAgreementStatus", CONF_RESOLVES_BOUND),
            ("Lab.legalAgreementStatus", CONF_RESOLVES_TYPED),
            ("LabRef.legalAgreementStatus", CONF_RESOLVES_TYPED),
        ] {
            assert_eq!(
                edge_between(&a, LEGAL, &at(c), EdgeKind::Resolves),
                Some(conf),
                "{c}"
            );
        }
        // Only the guarded type bound: the bare label still lands on the root
        // field, never back on the guarded coordinate.
        let mut cfg = config();
        cfg.1.relations = vec![rel(
            GET_RESOLVERS,
            EdgeKind::Binds,
            "Lab.legalAgreementStatus",
        )];
        let a = analyze(repo(), &[schema, lambda, cfg]);
        assert_eq!(
            edge_between(
                &a,
                LEGAL,
                &at("Query.legalAgreementStatus"),
                EdgeKind::Resolves
            ),
            Some(CONF_RESOLVES_UNIQUE)
        );
        assert_eq!(
            edge_between(
                &a,
                LEGAL,
                &at("Lab.legalAgreementStatus"),
                EdgeKind::Resolves
            ),
            Some(CONF_RESOLVES_TYPED)
        );
    }

    #[test]
    fn resolver_nodes_are_never_call_targets() {
        let mut caller = file(
            "src/x.ts",
            "export function main() { createLab(); }",
            vec![sym(
                "typescript:src/x.ts:function:main",
                NodeKind::Function,
                "main",
                None,
            )],
        );
        caller.1.calls = vec![call("typescript:src/x.ts:function:main", "createLab", None)];
        let mut files = schemas();
        files.push(lambda());
        files.push(caller);
        let a = analyze(repo(), &files);
        assert_eq!(a.stats.calls_resolved, 0);
        assert!(edges_of(&a, EdgeKind::Calls).is_empty());
        // The resolver is still a unit with its own chunk.
        assert!(a.units.iter().any(|u| u.symbol.stable_id == RESOLVER));
    }

    #[test]
    fn incremental_resolves_sees_persisted_bindings() {
        let mut files = schemas();
        files.push(lambda());
        files.push(config());
        let full = analyze(repo(), &files);
        assert_eq!(
            edge_between(&full, RESOLVER, MERGED, EdgeKind::Resolves),
            Some(CONF_RESOLVES_BOUND)
        );
        let known = Graph {
            files: full.bundle.files.clone(),
            nodes: full.bundle.nodes.clone(),
            edges: full.bundle.edges.clone(),
        };
        // `add` of the Lambda alone: the binding lives in the unchanged config.
        let inc = analyze_with(
            repo(),
            &[lambda()],
            Some(&known),
            &SolidityLayout::default(),
        );
        let resolver = node_by_stable(&inc, RESOLVER).id;
        let merged = node_by_stable(&full, MERGED).id;
        let e = inc
            .bundle
            .edges
            .iter()
            .find(|e| e.kind == EdgeKind::Resolves && e.source_node_id == resolver)
            .expect("resolves edge");
        assert_eq!(
            (e.target_node_id, e.confidence),
            (merged, CONF_RESOLVES_BOUND)
        );
        // `add` of the config: its fresh extraction replaces the persisted binding.
        let mut unbound = config();
        unbound.1.relations.clear();
        assert!(known_bound(&known, &[unbound]).is_empty());
        assert_eq!(known_bound(&known, &[lambda()]), ["Mutation.createLab"]);
    }

    #[test]
    fn file_node_records_bindings_and_lambda_entries() {
        let mut c = config();
        c.1.graphql_bindings = vec![
            GraphqlBinding {
                coordinate: "Mutation.createLab".into(),
                line: 666,
                owner: Some(GET_RESOLVERS.into()),
            },
            GraphqlBinding {
                coordinate: "Mutation.issueConsumerCredential".into(),
                line: 117,
                owner: None,
            },
        ];
        c.1.lambda_entries = vec!["lambda/labs/index.ts".into()];
        let a = analyze(repo(), &[c, lambda()]);
        let meta = &node_by_stable(&a, "file:lib/c.ts").metadata;
        assert_eq!(
            meta["graphql_bindings"],
            json!([
                {"coordinate": "Mutation.createLab", "line": 666, "owner": GET_RESOLVERS},
                {"coordinate": "Mutation.issueConsumerCredential", "line": 117, "owner": null},
            ])
        );
        assert_eq!(meta["lambda_entries"], json!(["lambda/labs/index.ts"]));
        let plain = &node_by_stable(&a, "file:lambda/labs/index.ts").metadata;
        assert!(plain.get("graphql_bindings").is_none());
        assert!(plain.get("lambda_entries").is_none());
        assert_eq!(plain["language"], "typescript");
    }

    fn ccall(
        member: Option<&str>,
        abi: &str,
        import: Option<&str>,
        owner: Option<&str>,
    ) -> ContractCall {
        ContractCall {
            member: member.map(str::to_string),
            kind: if member.is_some() {
                ContractMemberKind::Function
            } else {
                ContractMemberKind::Abi
            },
            abi: Some(abi.into()),
            abi_import: import.map(str::to_string),
            scope: AbiScope::Object,
            via: Some("readContract".into()),
            owner: owner.map(str::to_string),
            line: 3,
        }
    }

    const DECODE: &str = "typescript:lambda/utils/event.ts:function:decode";

    /// An ABI JSON file, a TS file with a local ABI and a combined ABI, a
    /// sibling ABI module it imports, and an unimported ABI elsewhere.
    fn abi_fixture() -> Vec<(FileInput, FileExtraction)> {
        let json = file(
            "lambda/assets/abis/AgentFactory.json",
            "[]",
            vec![sym(
                "abi:lambda/assets/abis/AgentFactory.json:abi:AgentFactory",
                NodeKind::Abi,
                "AgentFactory",
                None,
            )],
        );
        let mut combined = sym(
            "typescript:lambda/utils/event.ts:abi:COMBINED",
            NodeKind::Abi,
            "COMBINED",
            None,
        );
        combined.metadata = json!({"abi": {"source": "combined", "combines": [
            {"name": "AGENTFACTORY", "import": "../assets/abis/AgentFactory.json"},
            {"name": "SharedAbi"},
        ]}});
        let mut event = file(
            "lambda/utils/event.ts",
            "import AGENTFACTORY from '../assets/abis/AgentFactory.json';\n",
            vec![
                sym(DECODE, NodeKind::Function, "decode", None),
                sym(
                    "typescript:lambda/utils/event.ts:abi:LOCAL_ABI",
                    NodeKind::Abi,
                    "LOCAL_ABI",
                    None,
                ),
                combined,
            ],
        );
        event.1.imports = vec![
            imp("../assets/abis/AgentFactory.json", &[]),
            imp("./abis", &["SharedAbi"]),
        ];
        event.1.contract_calls = vec![
            ccall(
                None,
                "AGENTFACTORY",
                Some("../assets/abis/AgentFactory.json"),
                None,
            ),
            ccall(Some("a"), "LOCAL_ABI", None, Some(DECODE)),
            ccall(Some("b"), "SharedAbi", None, Some(DECODE)),
            ccall(Some("c"), "LoneAbi", None, Some(DECODE)),
            ccall(Some("d"), "AGENT_FACTORY", None, Some(DECODE)),
            ccall(Some("balanceOf"), "erc20Abi", None, Some(DECODE)),
        ];
        let shared = file(
            "lambda/utils/abis.ts",
            "",
            vec![sym(
                "typescript:lambda/utils/abis.ts:abi:SharedAbi",
                NodeKind::Abi,
                "SharedAbi",
                None,
            )],
        );
        let lone = file(
            "src/other.ts",
            "",
            vec![sym(
                "typescript:src/other.ts:abi:LoneAbi",
                NodeKind::Abi,
                "LoneAbi",
                None,
            )],
        );
        vec![json, event, shared, lone]
    }

    #[test]
    fn contract_calls_land_on_file_metadata() {
        let files = abi_fixture();
        let a = analyze(repo(), &files);
        let f = node_by_stable(&a, "file:lambda/utils/event.ts");
        let back: Vec<ContractCall> =
            serde_json::from_value(f.metadata["contract_calls"].clone()).unwrap();
        assert_eq!(back, files[1].1.contract_calls);
        let other = node_by_stable(&a, "file:src/other.ts");
        assert!(other.metadata.get("contract_calls").is_none());
    }

    #[test]
    fn uses_abi_resolves_json_default_import_then_name_ladder() {
        let a = analyze(repo(), &abi_fixture());
        let uses = |from: &str, to: &str| edge_between(&a, from, to, EdgeKind::UsesAbi);
        let json = "abi:lambda/assets/abis/AgentFactory.json:abi:AgentFactory";
        assert_eq!(
            uses("file:lambda/utils/event.ts", json),
            Some(CONF_SAME_FILE)
        );
        assert_eq!(
            uses(DECODE, "typescript:lambda/utils/event.ts:abi:LOCAL_ABI"),
            Some(CONF_SAME_FILE)
        );
        assert_eq!(
            uses(DECODE, "typescript:lambda/utils/abis.ts:abi:SharedAbi"),
            Some(CONF_IMPORTED)
        );
        assert_eq!(
            uses(DECODE, "typescript:src/other.ts:abi:LoneAbi"),
            Some(CONF_REPO_UNIQUE)
        );
        assert_eq!(uses(DECODE, json), Some(CONF_ABI_NAME_KEY));
        // erc20Abi (viem's) has no definition here: no edge; 2 combine parts resolve.
        assert_eq!(a.stats.abi_uses_total, 8);
        assert_eq!(a.stats.abi_uses_resolved, 7);
        assert_eq!(edges_of(&a, EdgeKind::UsesAbi).len(), 7);
    }

    #[test]
    fn combined_abi_points_at_each_part() {
        let a = analyze(repo(), &abi_fixture());
        let from = "typescript:lambda/utils/event.ts:abi:COMBINED";
        assert_eq!(
            edge_between(
                &a,
                from,
                "abi:lambda/assets/abis/AgentFactory.json:abi:AgentFactory",
                EdgeKind::UsesAbi
            ),
            Some(CONF_SAME_FILE)
        );
        assert_eq!(
            edge_between(
                &a,
                from,
                "typescript:lambda/utils/abis.ts:abi:SharedAbi",
                EdgeKind::UsesAbi
            ),
            Some(CONF_IMPORTED)
        );
    }

    #[test]
    fn abi_symbols_never_join_call_resolution() {
        let mut caller = file(
            "src/a.ts",
            "getRole();",
            vec![sym(
                "typescript:src/a.ts:function:main",
                NodeKind::Function,
                "main",
                None,
            )],
        );
        caller.1.calls = vec![call("typescript:src/a.ts:function:main", "getRole", None)];
        let def = file(
            "src/b.ts",
            "",
            vec![sym(
                "typescript:src/b.ts:function:getRole",
                NodeKind::Function,
                "getRole",
                None,
            )],
        );
        let abi = file(
            "src/abis/getRole.json",
            "[]",
            vec![sym(
                "abi:src/abis/getRole.json:abi:getRole",
                NodeKind::Abi,
                "getRole",
                None,
            )],
        );
        let a = analyze(repo(), &[caller, def, abi]);
        assert_eq!(
            edge_between(
                &a,
                "typescript:src/a.ts:function:main",
                "typescript:src/b.ts:function:getRole",
                EdgeKind::Calls
            ),
            Some(CONF_REPO_UNIQUE)
        );
        // An Abi is a unit (it gets a chunk), never a call target.
        assert!(a.units.iter().any(|u| u.symbol.kind == NodeKind::Abi));
        assert_eq!(edges_of(&a, EdgeKind::Calls).len(), 1);
    }

    #[test]
    fn uses_abi_with_known_graph_matches_full_analyze() {
        let files = abi_fixture();
        let full = analyze(repo(), &files);
        let event_ids: HashSet<Uuid> = full
            .bundle
            .nodes
            .iter()
            .filter(|n| n.file_id == Some(full.bundle.files[1].id))
            .map(|n| n.id)
            .collect();
        let out_edges = |a: &Analyzed| -> Vec<(Uuid, Uuid, u32)> {
            let mut v: Vec<_> = a
                .bundle
                .edges
                .iter()
                .filter(|e| e.kind == EdgeKind::UsesAbi && event_ids.contains(&e.source_node_id))
                .map(|e| (e.source_node_id, e.target_node_id, e.confidence.to_bits()))
                .collect();
            v.sort();
            v
        };
        let mut known = Graph {
            files: full.bundle.files.clone(),
            nodes: full.bundle.nodes.clone(),
            edges: full.bundle.edges.clone(),
        };
        known.files.retain(|f| f.path != "lambda/utils/event.ts");
        known.nodes.retain(|n| !event_ids.contains(&n.id));
        let inc = analyze_with(
            repo(),
            std::slice::from_ref(&files[1]),
            Some(&known),
            &SolidityLayout::default(),
        );
        assert_eq!(out_edges(&inc), out_edges(&full));
        assert_eq!(out_edges(&full).len(), 7);
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
