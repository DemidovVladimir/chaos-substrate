//! GraphQL entry anchoring for `feature`: when the question (or the explicit
//! `entry` argument) names a GraphQL field, the answer leads with that field's
//! path through the code, read from the persisted graph:
//!
//! 1. the SDL copies of the field (the canonical copy first: the one the
//!    `queries` / `binds` / `resolves` edges land on);
//! 2. the resolver registrations that `binds` it (AppSync `getResolvers()`
//!    entries), with the Lambda handler files their file deploys;
//! 3. the `Resolver`s that `resolves` it (a `switch (fieldName)` case);
//! 4. the `calls` behind those resolvers, breadth-first, up to [`ENTRY_HOPS`];
//! 5. the operations selecting it (`metadata.root_fields`, test files left
//!    out) and the React hooks that reach their host within
//!    [`HOOK_HOPS`] reverse call hops.
//!
//! Coordinates come from [`coordinates`]: `Type.field` written in the question
//! always anchors; a bare camelCase name only next to a cue word ([`CUE_WORDS`])
//! and then as `Query.x` / `Mutation.x` / `Subscription.x`; the explicit
//! `entry` argument anchors on its own. Nothing anchors unless the store has
//! the field or an operation selecting it. Ports only.
//!
//! The anchor is emitted once, in `FeatureAnswer.entry` (its `binds`,
//! `resolves` and `calls` edges are not repeated in `graph`), and is charged
//! to the token budget in full: [`EntryTrace::fit`] trims it to
//! [`ENTRY_BUDGET_SHARE`] of the budget before the rest is packed.

use super::entry_points::{is_test_path, HOOK_HOPS};
use super::feature::{literal_terms, node_path, node_symbol, GraphEdge};
use crate::domain::hash;
use crate::domain::model::{Edge, EdgeKind, Node, Repo};
use crate::ports::search_store::{Hit, SearchStore};
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use uuid::Uuid;

/// Call hops followed from a resolver.
pub const ENTRY_HOPS: usize = 3;
/// Call edges kept in the trace (hop order first, so the cut drops the deepest).
pub const ENTRY_MAX_TRACE_EDGES: usize = 30;
/// Code chunks the anchor contributes to the excerpts.
pub const ENTRY_MAX_CHUNKS: usize = 8;
/// Calling operations listed.
pub const ENTRY_MAX_CALLERS: usize = 20;
/// Share of a `feature` budget the serialized anchor may take (see
/// [`EntryTrace::fit`]); it is charged before the rest is packed.
pub const ENTRY_BUDGET_SHARE: f64 = 0.25;
/// Words that make a bare field name in the question a GraphQL entry.
pub const CUE_WORDS: [&str; 6] = [
    "query",
    "mutation",
    "subscription",
    "resolver",
    "graphql",
    "field",
];
const MAX_COORDINATES: usize = 6;
const ROOTS: [&str; 3] = ["Query", "Mutation", "Subscription"];
const BUILTIN_SCALARS: [&str; 5] = ["String", "Int", "Float", "Boolean", "ID"];
/// Node kinds an SDL named type may have, in stable-id spelling.
const SDL_TYPE_KINDS: [&str; 4] = ["type", "interface", "enum", "type_alias"];

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SdlField {
    pub coordinate: String,
    pub path: String,
    pub line: i32,
    /// One-line `name(args): Type @directives`.
    pub header: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EntryBinding {
    pub coordinate: String,
    /// The registering symbol (`AppSyncLabsLambdaConfig::getResolvers`), or
    /// `(file)` for a top-level registration.
    pub symbol: String,
    pub path: String,
    pub line: i32,
    /// Handler files the registering file deploys (CDK `entry:`).
    pub lambda_entries: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EntryResolver {
    pub coordinate: String,
    pub symbol: String,
    pub path: String,
    pub lines: [i32; 2],
    /// The `resolves` edge confidence (0.9 typed or bound, 0.7 name-only).
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TraceEdge {
    pub from: String,
    pub kind: String,
    pub to: String,
    pub from_path: String,
    pub to_path: String,
    /// 1 = called by a resolver directly.
    pub hop: usize,
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EntryCaller {
    pub coordinate: String,
    /// Operation name.
    pub op: String,
    pub path: String,
    pub line: i32,
    /// React hooks reaching the operation's host function.
    pub hooks: Vec<String>,
}

/// What the store knows about the anchored GraphQL fields (module docs).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EntryTrace {
    pub coordinates: Vec<String>,
    pub sdl: Vec<SdlField>,
    pub bindings: Vec<EntryBinding>,
    pub resolvers: Vec<EntryResolver>,
    pub trace: Vec<TraceEdge>,
    pub callers: Vec<EntryCaller>,
}

impl EntryTrace {
    /// `binds`, `resolves`, then the `calls` trace, as feature graph edges.
    pub fn graph_edges(&self) -> Vec<GraphEdge> {
        let sdl_path = |c: &str| {
            self.sdl
                .iter()
                .find(|s| s.coordinate == c)
                .map(|s| s.path.clone())
                .unwrap_or_default()
        };
        let mut out: Vec<GraphEdge> = Vec::new();
        for b in &self.bindings {
            out.push(GraphEdge {
                from: b.symbol.clone(),
                kind: EdgeKind::Binds.as_str().into(),
                to: b.coordinate.clone(),
                from_path: b.path.clone(),
                to_path: sdl_path(&b.coordinate),
            });
        }
        for r in &self.resolvers {
            out.push(GraphEdge {
                from: r.symbol.clone(),
                kind: EdgeKind::Resolves.as_str().into(),
                to: r.coordinate.clone(),
                from_path: r.path.clone(),
                to_path: sdl_path(&r.coordinate),
            });
        }
        for t in &self.trace {
            out.push(GraphEdge {
                from: t.from.clone(),
                kind: t.kind.clone(),
                to: t.to.clone(),
                from_path: t.from_path.clone(),
                to_path: t.to_path.clone(),
            });
        }
        let mut seen = BTreeSet::new();
        out.retain(|g| seen.insert(g.clone()));
        out
    }

    /// Serialized size: what the answer emits for the anchor.
    pub fn chars(&self) -> usize {
        serde_json::to_string(self).map_or(0, |s| s.len())
    }

    /// Trim to at most `cap` serialized chars, least telling rows first:
    /// call-trace edges past hop 1 (deepest first), SDL copies after the
    /// first of each coordinate, calling operations, then hop-1 edges (all
    /// from the tail). The coordinates, the first SDL copy of each, the
    /// registrations and the resolvers are the anchor itself and are never
    /// cut, so the result can exceed `cap`. Returns the size after trimming.
    pub fn fit(&mut self, cap: usize) -> usize {
        loop {
            let size = self.chars();
            if size <= cap {
                return size;
            }
            let extra_sdl = (1..self.sdl.len()).rev().find(|&i| {
                self.sdl[..i]
                    .iter()
                    .any(|s| s.coordinate == self.sdl[i].coordinate)
            });
            if self.trace.last().is_some_and(|t| t.hop > 1) {
                self.trace.pop();
            } else if let Some(i) = extra_sdl {
                self.sdl.remove(i);
            } else if self.callers.pop().is_none() && self.trace.pop().is_none() {
                return size;
            }
        }
    }

    /// One provenance line, e.g. `Mutation.createLab: 2 SDL copies, 1 binding, …`.
    pub fn describe(&self) -> String {
        format!(
            "{}: {} SDL copies, {} bindings, {} resolvers, {} call edges ≤ {ENTRY_HOPS} hops, {} callers",
            self.coordinates.join(", "),
            self.sdl.len(),
            self.bindings.len(),
            self.resolvers.len(),
            self.trace.len(),
            self.callers.len()
        )
    }
}

/// `Type.field`: an upper-case type name, a dot, a lower-case field name.
fn is_coordinate(t: &str) -> bool {
    let Some((ty, field)) = t.split_once('.') else {
        return false;
    };
    let word = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    word(ty)
        && word(field)
        && ty.starts_with(|c: char| c.is_ascii_uppercase())
        && field.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
}

/// A bare field name: lower-case start, GraphQL name characters only.
fn is_bare_field(t: &str) -> bool {
    t.starts_with(|c: char| c.is_ascii_lowercase())
        && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn has_cue(question: &str) -> bool {
    question
        .split(|c: char| !c.is_ascii_alphanumeric())
        .map(str::to_ascii_lowercase)
        .any(|w| {
            CUE_WORDS
                .iter()
                .any(|c| w == *c || w == format!("{c}s") || (*c == "query" && w == "queries"))
        })
}

/// The GraphQL coordinates a question anchors on (module docs); at most 6.
pub fn coordinates(question: &str, entry: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |c: String| {
        if !out.contains(&c) {
            out.push(c);
        }
    };
    let roots = |f: &str| ROOTS.map(|r| format!("{r}.{f}"));
    if let Some(e) = entry.map(str::trim).filter(|e| !e.is_empty()) {
        if e.contains('.') {
            push(e.to_string());
        } else if is_bare_field(e) {
            roots(e).into_iter().for_each(&mut push);
        }
    } else {
        let cue = has_cue(question);
        for t in literal_terms(question) {
            if is_coordinate(&t) {
                push(t);
            } else if cue && is_bare_field(&t) {
                roots(&t).into_iter().for_each(&mut push);
            }
        }
    }
    out.truncate(MAX_COORDINATES);
    out
}

/// An edge confidence as written in answers: two decimals (`0.9`, not `0.8999999761581421`).
fn round2(c: f32) -> f64 {
    (f64::from(c) * 100.0).round() / 100.0
}

fn meta_str<'a>(n: &'a Node, key: &str) -> &'a str {
    n.metadata[key].as_str().unwrap_or("")
}

fn qualified(n: &Node) -> &str {
    match meta_str(n, "qualified_name") {
        "" => n.name.as_str(),
        q => q,
    }
}

/// Named types an SDL field header mentions as argument or return types
/// (`createLab(input: CreateLabInput!): CreateLabResult!` → both), minus the
/// built-in and `AWS*` scalars.
pub fn header_types(header: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let chars: Vec<char> = header.chars().collect();
    let (mut parens, mut braces) = (0i32, 0i32);
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '(' => parens += 1,
            ')' => parens -= 1,
            '{' => braces += 1,
            '}' => braces -= 1,
            '@' if parens == 0 && braces == 0 => break,
            ':' if parens <= 1 && braces == 0 => {
                let mut j = i + 1;
                while j < chars.len() && (chars[j] == ' ' || chars[j] == '[') {
                    j += 1;
                }
                let name: String = chars[j..]
                    .iter()
                    .take_while(|c| c.is_ascii_alphanumeric() || **c == '_')
                    .collect();
                if !name.is_empty()
                    && !BUILTIN_SCALARS.contains(&name.as_str())
                    && !name.starts_with("AWS")
                    && !out.contains(&name)
                {
                    out.push(name);
                }
                i = j;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// Trace the anchored coordinates through the store (module docs). `None`
/// when the repository has neither an SDL copy of them nor an operation
/// selecting them. The hits are the anchor's code chunks in reading order:
/// resolvers, their direct callees, then the SDL types of the field.
pub async fn trace(
    search: &dyn SearchStore,
    repo: &Repo,
    coords: &[String],
) -> Result<Option<(EntryTrace, Vec<Hit>)>> {
    if coords.is_empty() {
        return Ok(None);
    }
    let fields = search
        .graphql_fields(repo.id, coords)
        .await
        .context("graphql fields")?;
    let mut ops = search
        .operations_selecting(repo.id, coords)
        .await
        .context("operations selecting")?;
    ops.retain(|o| !is_test_path(meta_str(o, "path")));
    if fields.is_empty() && ops.is_empty() {
        return Ok(None);
    }
    let field_ids: Vec<Uuid> = fields.iter().map(|f| f.id).collect();
    let coordinate_of: HashMap<Uuid, &str> = fields.iter().map(|f| (f.id, qualified(f))).collect();
    let mut into: Vec<Edge> = if field_ids.is_empty() {
        Vec::new()
    } else {
        search
            .edges_for_nodes(repo.id, &field_ids)
            .await
            .context("edges into fields")?
    };
    into.retain(|e| {
        coordinate_of.contains_key(&e.target_node_id)
            && matches!(
                e.kind,
                EdgeKind::Binds | EdgeKind::Resolves | EdgeKind::Queries
            )
    });

    // 1. SDL copies: per coordinate, the copy the edges land on first.
    let canonical: HashSet<Uuid> = into.iter().map(|e| e.target_node_id).collect();
    let rank = |c: &str| coords.iter().position(|x| x == c).unwrap_or(usize::MAX);
    let mut sdl_nodes: Vec<&Node> = fields.iter().collect();
    sdl_nodes.sort_by_key(|n| {
        (
            rank(qualified(n)),
            !canonical.contains(&n.id),
            meta_str(n, "path").to_string(),
            n.line_start,
        )
    });
    let sdl: Vec<SdlField> = sdl_nodes
        .iter()
        .map(|n| SdlField {
            coordinate: qualified(n).to_string(),
            path: meta_str(n, "path").to_string(),
            line: n.line_start.unwrap_or(0),
            header: match meta_str(n, "header") {
                "" => n.signature.clone().unwrap_or_default(),
                h => h.to_string(),
            },
            doc: n.doc.clone(),
        })
        .collect();

    // 2 + 3. Registrations and resolvers.
    let sources: Vec<Uuid> = {
        let mut v: Vec<Uuid> = into
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::Binds | EdgeKind::Resolves))
            .map(|e| e.source_node_id)
            .collect();
        v.sort();
        v.dedup();
        v
    };
    let mut nodes: HashMap<Uuid, Node> = HashMap::new();
    fetch(search, &sources, &mut nodes).await?;
    let path_of = |n: &Node| -> String {
        if n.stable_id.starts_with("file:") {
            n.name.clone()
        } else {
            match meta_str(n, "path") {
                "" => node_path(n),
                p => p.to_string(),
            }
        }
    };
    let file_nodes: Vec<Uuid> = into
        .iter()
        .filter(|e| e.kind == EdgeKind::Binds)
        .filter_map(|e| nodes.get(&e.source_node_id))
        .map(|n| hash::scoped_id(repo.id, &format!("file:{}", path_of(n))))
        .collect();
    fetch(search, &file_nodes, &mut nodes).await?;
    let mut bindings: Vec<EntryBinding> = Vec::new();
    let mut resolvers: Vec<EntryResolver> = Vec::new();
    for e in &into {
        let (Some(src), Some(coordinate)) = (
            nodes.get(&e.source_node_id),
            coordinate_of.get(&e.target_node_id),
        ) else {
            continue;
        };
        let path = path_of(src);
        match e.kind {
            EdgeKind::Binds => {
                let is_file = src.stable_id.starts_with("file:");
                let file = nodes.get(&hash::scoped_id(repo.id, &format!("file:{path}")));
                let owner = (!is_file).then_some(src.stable_id.as_str());
                let line = file
                    .and_then(|f| f.metadata["graphql_bindings"].as_array())
                    .into_iter()
                    .flatten()
                    .find(|b| {
                        b["coordinate"].as_str() == Some(coordinate) && b["owner"].as_str() == owner
                    })
                    .and_then(|b| b["line"].as_i64())
                    .map(|l| l as i32)
                    .or(src.line_start)
                    .unwrap_or(0);
                let binding = EntryBinding {
                    coordinate: coordinate.to_string(),
                    symbol: if is_file {
                        "(file)".to_string()
                    } else {
                        node_symbol(src)
                    },
                    path,
                    line,
                    lambda_entries: file
                        .and_then(|f| f.metadata["lambda_entries"].as_array())
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect(),
                };
                if !bindings.contains(&binding) {
                    bindings.push(binding);
                }
            }
            EdgeKind::Resolves => resolvers.push(EntryResolver {
                coordinate: coordinate.to_string(),
                symbol: node_symbol(src),
                path,
                lines: [src.line_start.unwrap_or(0), src.line_end.unwrap_or(0)],
                confidence: round2(e.confidence),
            }),
            _ => {}
        }
    }
    bindings.sort_by(|a, b| {
        (rank(&a.coordinate), &a.path, a.line).cmp(&(rank(&b.coordinate), &b.path, b.line))
    });
    resolvers.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| (&a.path, a.lines).cmp(&(&b.path, b.lines)))
    });
    resolvers.dedup_by(|a, b| a.coordinate == b.coordinate && a.symbol == b.symbol);
    let resolver_ids: Vec<Uuid> = {
        let by_symbol: HashMap<(String, String), Uuid> = into
            .iter()
            .filter(|e| e.kind == EdgeKind::Resolves)
            .filter_map(|e| nodes.get(&e.source_node_id))
            .map(|n| ((node_symbol(n), path_of(n)), n.id))
            .collect();
        let mut v: Vec<Uuid> = Vec::new();
        for r in &resolvers {
            if let Some(id) = by_symbol.get(&(r.symbol.clone(), r.path.clone())) {
                if !v.contains(id) {
                    v.push(*id);
                }
            }
        }
        v
    };

    // 4. Calls behind the resolvers, breadth-first.
    let mut hops: Vec<(usize, Edge)> = Vec::new();
    let mut seen: HashSet<Uuid> = resolver_ids.iter().copied().collect();
    let mut frontier: Vec<Uuid> = resolver_ids.clone();
    for hop in 1..=ENTRY_HOPS {
        if frontier.is_empty() || hops.len() >= ENTRY_MAX_TRACE_EDGES {
            break;
        }
        let from: HashSet<Uuid> = frontier.iter().copied().collect();
        let mut es = search
            .edges_for_nodes(repo.id, &frontier)
            .await
            .context("call trace")?;
        es.retain(|e| e.kind == EdgeKind::Calls && from.contains(&e.source_node_id));
        es.sort_by_key(|e| (e.source_node_id, e.target_node_id));
        es.dedup_by_key(|e| (e.source_node_id, e.target_node_id));
        let mut next = Vec::new();
        for e in es {
            if seen.insert(e.target_node_id) {
                next.push(e.target_node_id);
            }
            hops.push((hop, e));
        }
        frontier = next;
    }
    let ends: Vec<Uuid> = hops
        .iter()
        .flat_map(|(_, e)| [e.source_node_id, e.target_node_id])
        .collect();
    fetch(search, &ends, &mut nodes).await?;
    let stable = |id: &Uuid| {
        nodes
            .get(id)
            .map(|n| n.stable_id.clone())
            .unwrap_or_default()
    };
    hops.sort_by(|(ha, a), (hb, b)| {
        ha.cmp(hb)
            .then_with(|| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| stable(&a.target_node_id).cmp(&stable(&b.target_node_id)))
            .then_with(|| stable(&a.source_node_id).cmp(&stable(&b.source_node_id)))
    });
    hops.truncate(ENTRY_MAX_TRACE_EDGES);
    let trace: Vec<TraceEdge> = hops
        .iter()
        .filter_map(|(hop, e)| {
            let (f, t) = (nodes.get(&e.source_node_id)?, nodes.get(&e.target_node_id)?);
            Some(TraceEdge {
                from: node_symbol(f),
                kind: e.kind.as_str().into(),
                to: node_symbol(t),
                from_path: path_of(f),
                to_path: path_of(t),
                hop: *hop,
                confidence: round2(e.confidence),
            })
        })
        .collect();

    // 5. Callers: operations, their host symbol, hooks reaching the host.
    let callers = callers(search, repo, coords, &ops, &mut nodes).await?;

    // Chunks: resolvers, direct callees, then the field's SDL types.
    let mut chunk_nodes: Vec<Uuid> = resolver_ids.clone();
    for (hop, e) in &hops {
        if *hop == 1 && !chunk_nodes.contains(&e.target_node_id) {
            chunk_nodes.push(e.target_node_id);
        }
    }
    let mut type_paths: HashMap<Uuid, String> = HashMap::new();
    let mut seen_coordinate = HashSet::new();
    for s in &sdl {
        if !seen_coordinate.insert(s.coordinate.as_str()) {
            continue; // only the canonical copy's types
        }
        for ty in header_types(&s.header) {
            for kind in SDL_TYPE_KINDS {
                let id = hash::scoped_id(repo.id, &format!("graphql:{}:{kind}:{ty}", s.path));
                if !chunk_nodes.contains(&id) {
                    chunk_nodes.push(id);
                    type_paths.insert(id, s.path.clone());
                }
            }
        }
    }
    let mut chunks = if chunk_nodes.is_empty() {
        Vec::new()
    } else {
        search
            .chunks_for_nodes(&chunk_nodes)
            .await
            .context("entry chunks")?
    };
    let order: HashMap<Uuid, usize> = chunk_nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (*n, i))
        .collect();
    chunks.sort_by_key(|c| {
        (
            c.node_id
                .and_then(|n| order.get(&n).copied())
                .unwrap_or(usize::MAX),
            c.line_start,
            c.id,
        )
    });
    chunks.dedup_by_key(|c| c.id);
    chunks.truncate(ENTRY_MAX_CHUNKS);
    let hits: Vec<Hit> = chunks
        .into_iter()
        .map(|c| {
            let path = c
                .node_id
                .and_then(|n| {
                    nodes
                        .get(&n)
                        .map(path_of)
                        .or_else(|| type_paths.get(&n).cloned())
                })
                .unwrap_or_default();
            Hit {
                chunk: c,
                path,
                score: 0.0,
            }
        })
        .collect();

    // Only the coordinates the store knows (a bare name asks for all three roots).
    let known: Vec<String> = coords
        .iter()
        .filter(|c| {
            sdl.iter().any(|s| &s.coordinate == *c)
                || ops.iter().any(|o| {
                    o.metadata["root_fields"]
                        .as_array()
                        .is_some_and(|f| f.iter().any(|f| f.as_str() == Some(c.as_str())))
                })
        })
        .cloned()
        .collect();
    Ok(Some((
        EntryTrace {
            coordinates: known,
            sdl,
            bindings,
            resolvers,
            trace,
            callers,
        },
        hits,
    )))
}

/// Load the nodes of `ids` not loaded yet into `nodes`.
async fn fetch(
    search: &dyn SearchStore,
    ids: &[Uuid],
    nodes: &mut HashMap<Uuid, Node>,
) -> Result<()> {
    let mut missing: Vec<Uuid> = ids
        .iter()
        .copied()
        .filter(|id| !nodes.contains_key(id))
        .collect();
    missing.sort();
    missing.dedup();
    if missing.is_empty() {
        return Ok(());
    }
    for n in search.nodes_by_ids(&missing).await.context("entry nodes")? {
        nodes.insert(n.id, n);
    }
    Ok(())
}

/// The operations selecting the coordinates, each with the hooks that reach
/// its host symbol (the symbol `defines` the operation) within [`HOOK_HOPS`]
/// reverse call hops. One row per (operation, coordinate), at most
/// [`ENTRY_MAX_CALLERS`].
async fn callers(
    search: &dyn SearchStore,
    repo: &Repo,
    coords: &[String],
    ops: &[Node],
    nodes: &mut HashMap<Uuid, Node>,
) -> Result<Vec<EntryCaller>> {
    if ops.is_empty() {
        return Ok(Vec::new());
    }
    let op_ids: Vec<Uuid> = ops.iter().map(|o| o.id).collect();
    let op_set: HashSet<Uuid> = op_ids.iter().copied().collect();
    let defines = search
        .edges_for_nodes(repo.id, &op_ids)
        .await
        .context("operation hosts")?;
    let mut host_of: HashMap<Uuid, Uuid> = HashMap::new();
    for e in defines
        .iter()
        .filter(|e| e.kind == EdgeKind::Defines && op_set.contains(&e.target_node_id))
    {
        host_of.entry(e.target_node_id).or_insert(e.source_node_id);
    }
    let hosts: Vec<Uuid> = host_of.values().copied().collect();
    fetch(search, &hosts, nodes).await?;
    // node → the hosts it reaches by calls (reverse walk from the hosts).
    let mut reaches: HashMap<Uuid, BTreeSet<Uuid>> = HashMap::new();
    let mut frontier: Vec<Uuid> = Vec::new();
    for h in &hosts {
        if nodes
            .get(h)
            .is_some_and(|n| !n.stable_id.starts_with("file:"))
        {
            reaches.entry(*h).or_default().insert(*h);
            if !frontier.contains(h) {
                frontier.push(*h);
            }
        }
    }
    for _ in 0..HOOK_HOPS {
        if frontier.is_empty() {
            break;
        }
        let to: HashSet<Uuid> = frontier.iter().copied().collect();
        let mut es = search
            .edges_for_nodes(repo.id, &frontier)
            .await
            .context("hook walk")?;
        es.retain(|e| e.kind == EdgeKind::Calls && to.contains(&e.target_node_id));
        es.sort_by_key(|e| (e.target_node_id, e.source_node_id));
        let mut next = Vec::new();
        for e in es {
            let from = reaches.get(&e.target_node_id).cloned().unwrap_or_default();
            let entry = reaches.entry(e.source_node_id).or_default();
            let before = entry.len();
            entry.extend(from);
            if entry.len() > before && !next.contains(&e.source_node_id) {
                next.push(e.source_node_id);
            }
        }
        frontier = next;
    }
    let walked: Vec<Uuid> = reaches.keys().copied().collect();
    fetch(search, &walked, nodes).await?;
    let mut hooks_of: HashMap<Uuid, BTreeSet<String>> = HashMap::new();
    for (n, hs) in &reaches {
        let Some(node) = nodes.get(n).filter(|x| x.metadata["hook"].is_object()) else {
            continue;
        };
        for h in hs {
            hooks_of.entry(*h).or_default().insert(node.name.clone());
        }
    }
    let mut out: Vec<EntryCaller> = Vec::new();
    for op in ops {
        let hooks: Vec<String> = host_of
            .get(&op.id)
            .and_then(|h| hooks_of.get(h))
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        for f in op.metadata["root_fields"].as_array().into_iter().flatten() {
            let Some(f) = f.as_str().filter(|f| coords.iter().any(|c| c == f)) else {
                continue;
            };
            out.push(EntryCaller {
                coordinate: f.to_string(),
                op: op.name.clone(),
                path: meta_str(op, "path").to_string(),
                line: op.line_start.unwrap_or(0),
                hooks: hooks.clone(),
            });
        }
    }
    let rank = |c: &str| coords.iter().position(|x| x == c).unwrap_or(usize::MAX);
    out.sort_by(|a, b| {
        (rank(&a.coordinate), &a.path, a.line, &a.op).cmp(&(
            rank(&b.coordinate),
            &b.path,
            b.line,
            &b.op,
        ))
    });
    out.dedup();
    out.truncate(ENTRY_MAX_CALLERS);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn coordinates_from_question_and_entry() {
        assert_eq!(
            coordinates("Mutation.createLab", None),
            s(&["Mutation.createLab"])
        );
        let triple = s(&[
            "Query.createLab",
            "Mutation.createLab",
            "Subscription.createLab",
        ]);
        assert_eq!(coordinates("createLab mutation resolver", None), triple);
        assert_eq!(
            coordinates("which queries call listLabMembers?", None).len(),
            3
        );
        assert!(coordinates("how does createLab work", None).is_empty());
        assert_eq!(coordinates("anything", Some("createLab")), triple);
        assert_eq!(
            coordinates("ignored Query.labs", Some(" Lab.members ")),
            s(&["Lab.members"])
        );
        // Not GraphQL-shaped: file paths, `a::b`, lower-case types.
        assert!(coordinates("src/lab.ts and Store::replace", None).is_empty());
        assert!(coordinates("graphql field lab.members", None).is_empty());
        assert_eq!(
            coordinates(
                "Query.a1 Query.b1 Query.c1 Query.d1 Query.e1 Query.f1 Query.g1",
                None
            )
            .len(),
            6
        );
    }

    #[test]
    fn header_types_skip_scalars_and_directives() {
        assert_eq!(
            header_types("createLab(input: CreateLabInput!, dryRun: Boolean = false): CreateLabResult! @aws_lambda @aws_cognito_user_pools(cognito_groups: [\"Admin\"])"),
            s(&["CreateLabInput", "CreateLabResult"])
        );
        assert_eq!(
            header_types("labs(filter: LabFilter = {kind: OPEN}, at: AWSDateTime): [Lab!]!"),
            s(&["LabFilter", "Lab"])
        );
        assert!(header_types("ping: String").is_empty());
    }
}
