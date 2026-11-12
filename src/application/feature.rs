//! Use-case `feature`: answer ONE feature question from the persisted index
//! without reading source files.
//!
//! Pipeline: embed the question once → route to the best-matching feature
//! communities by summary embedding → GraphQL entry anchor: when the question
//! (or `FeatureRequest.entry`) names a GraphQL field the repository defines or
//! calls, trace it through the graph ([`super::graphql_entry`]) and trim it to
//! its share of the budget → fused chunk
//! retrieval: literal identifiers named in the question over the whole repo,
//! then semantic and keyword over the routed files plus the files of those
//! literal hits and of the anchor (an exact identifier outranks summary
//! routing) → the anchor's chunks lead the excerpts → 1-hop graph expansion
//! of the top hits (minus the anchor's own edges) → env-var / HTTP-route
//! surfaces of the involved files → pack the rest under what the anchor left
//! of the token budget (`tokens_used` counts the anchor too) → optionally
//! persist the answer as a hash-gated page. Deterministic given the store
//! outputs.

use super::graphql_entry::{self, EntryTrace, ENTRY_BUDGET_SHARE};
use crate::domain::budget::{self, ExcerptMode, Item};
use crate::domain::fusion::{self, Candidate, Channel};
use crate::domain::hash;
use crate::domain::model::{Chunk, Edge, EdgeKind, Node, NodeKind, Repo};
use crate::domain::provenance::{source, Breadcrumb};
use crate::ports::community_store::CommunityStore;
use crate::ports::embedder::Embedder;
use crate::ports::pages::{PageKind, PageStore};
use crate::ports::search_store::{Hit, SearchStore};
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Community routing: keep summaries scoring ≥ this cosine …
pub const ROUTE_FLOOR: f32 = 0.35;
/// … but never fewer than this many (when available) …
pub const ROUTE_MIN: usize = 2;
/// … nor more than this many.
pub const ROUTE_MAX: usize = 5;
const COMMUNITY_CANDIDATES: i64 = 5;
const SEMANTIC_LIMIT: i64 = 40;
const KEYWORD_LIMIT: i64 = 40;
const LITERAL_TOTAL: usize = 20;
const FUSE_LIMIT: usize = 30;
const EXPAND_TOP: usize = 8;
const EXPAND_MAX_CHUNKS: usize = 10;
const GRAPH_MAX_EDGES: usize = 40;
const SUMMARY_MAX: usize = 400;
/// Directory (relative to the repo root) where feature pages live.
pub const FEATURES_DIR: &str = "docs/features_memory";

pub const DEFAULT_BUDGET: usize = 6000;
pub const MIN_BUDGET: usize = 1000;
pub const MAX_BUDGET: usize = 16000;

/// Everything the use-case needs, as trait objects.
#[derive(Clone, Copy)]
pub struct Deps<'a> {
    pub search: &'a dyn SearchStore,
    pub communities: &'a dyn CommunityStore,
    pub embedder: &'a dyn Embedder,
    pub pages: &'a dyn PageStore,
}

#[derive(Debug, Clone)]
pub struct FeatureRequest {
    pub question: String,
    /// Clamped to `MIN_BUDGET..=MAX_BUDGET`.
    pub budget_tokens: usize,
    pub persist: bool,
    /// GraphQL field to start from: a coordinate (`Mutation.createLab`) or a
    /// root field name (`createLab`). A coordinate in the question anchors
    /// without it.
    pub entry: Option<String>,
}

impl FeatureRequest {
    pub fn new(question: impl Into<String>) -> Self {
        Self {
            question: question.into(),
            budget_tokens: DEFAULT_BUDGET,
            persist: true,
            entry: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FeatureHit {
    pub id: Uuid,
    pub label: String,
    pub score: f32,
    /// ≤ 400 chars; empty when the budget dropped it.
    pub summary: String,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Excerpt {
    pub path: String,
    pub lines: [i32; 2],
    pub symbol: String,
    pub kind: String,
    pub score: f32,
    /// `entry` / `semantic` / `keyword` / `literal` / `graph`.
    pub why: Vec<&'static str>,
    /// `header + "\n" + body`, clipped per the budget; header only when `code_truncated`
    /// and the body was dropped.
    pub code: String,
    pub code_truncated: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphEdge {
    pub from: String,
    pub kind: String,
    pub to: String,
    /// Repo-relative files of `from` / `to` (from the node stable ids).
    pub from_path: String,
    pub to_path: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct SurfaceHit {
    pub kind: String,
    pub name: String,
    pub path: String,
    pub line: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct FeatureAnswer {
    pub question: String,
    pub repo: String,
    pub features: Vec<FeatureHit>,
    pub excerpts: Vec<Excerpt>,
    pub graph: Vec<GraphEdge>,
    pub surfaces: Vec<SurfaceHit>,
    /// The GraphQL field the answer is anchored on (SDL, registration,
    /// resolver, call trace, callers), when the question names one. Its
    /// `binds` / `resolves` / `calls` edges live here only, never in `graph`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<EntryTrace>,
    /// Page path relative to the repo root (when persisted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    /// True when the page already carried this answer's `content_hash`.
    pub page_cached: bool,
    pub content_hash: String,
    pub tokens_used: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub provenance: Vec<Breadcrumb>,
}

/// Lower-case `[a-z0-9-]` slug, ≤ 80 chars, never empty.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut dash = true;
    for c in s.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
        if out.len() >= 80 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "page".to_string()
    } else {
        out
    }
}

/// Tokens of the question worth a literal substring search: ≥ 4 chars and
/// shaped like an identifier (snake_case, `a::b`, `a.b`, camelCase, digits
/// inside). Deduplicated, question order.
pub fn literal_terms(question: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for raw in question.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\'' | '`' | '?' | '!'
            )
    }) {
        let tok = raw.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        if tok.chars().count() < 4 || !looks_like_identifier(tok) {
            continue;
        }
        if seen.insert(tok.to_string()) {
            out.push(tok.to_string());
        }
    }
    out
}

pub(crate) fn looks_like_identifier(tok: &str) -> bool {
    if !tok
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.' | '-' | '/'))
    {
        return false;
    }
    if tok.contains('_') || tok.contains("::") {
        return true;
    }
    if tok.contains('.') || tok.contains('/') || tok.contains('-') {
        // `a.b` / `src/x.rs` / `kebab-case` — require alnum on both sides of a separator.
        return tok
            .split(['.', '/', '-'])
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric()));
    }
    let chars: Vec<char> = tok.chars().collect();
    let camel = chars
        .windows(2)
        .any(|w| w[0].is_ascii_lowercase() && w[1].is_ascii_uppercase());
    let digit_inside = chars[1..].iter().any(|c| c.is_ascii_digit())
        && chars.iter().any(|c| c.is_ascii_alphabetic());
    camel || digit_inside
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

/// Qualified symbol of a node: the tail of `{lang}:{path}:{kind}:{qualified}`
/// (or the path for `file:{path}`).
pub(crate) fn node_symbol(node: &Node) -> String {
    let parts: Vec<&str> = node.stable_id.splitn(4, ':').collect();
    match parts.as_slice() {
        [_, _, _, tail] if !tail.is_empty() => tail.to_string(),
        ["file", path] => path.to_string(),
        _ => node.name.clone(),
    }
}

/// Serialized size of a graph edge, charged against the budget.
fn edge_cost(g: &GraphEdge) -> usize {
    g.from.len() + g.kind.len() + g.to.len() + g.from_path.len() + g.to_path.len() + 48
}

/// Repo-relative file of a node: the `{path}` of `{lang}:{path}:{kind}:{qualified}`
/// (or of `file:{path}`); empty when the id has neither shape.
pub(crate) fn node_path(node: &Node) -> String {
    let parts: Vec<&str> = node.stable_id.splitn(4, ':').collect();
    match parts.as_slice() {
        [_, path, _, _] | ["file", path] => path.to_string(),
        _ => String::new(),
    }
}

/// Symbol for a chunk with no node (file head) — derived from its header.
fn header_symbol(chunk: &Chunk, path: &str) -> String {
    for line in chunk.header.lines() {
        if let Some(rest) = line.strip_prefix("// kind:") {
            if let Some((_, name)) = rest.split_once("name:") {
                let n = name.trim();
                if !n.is_empty() {
                    return n.to_string();
                }
            }
        }
    }
    path.to_string()
}

struct Candidates {
    hit_by_chunk: HashMap<Uuid, Hit>,
    channels: Vec<Vec<Candidate>>,
    counts: BTreeMap<&'static str, usize>,
    /// Files added to the routed scope because they contain a literal term.
    literal_files: usize,
}

async fn retrieve(
    deps: &Deps<'_>,
    repo: &Repo,
    question: &str,
    embedding: &[f32],
    file_ids: Option<&[Uuid]>,
    entry_files: &[Uuid],
) -> Result<Candidates> {
    let model = deps.embedder.model_ref();
    let mut hit_by_chunk: HashMap<Uuid, Hit> = HashMap::new();
    let mut counts = BTreeMap::new();
    let mut to_candidates = |hits: Vec<Hit>, channel: Channel| -> Vec<Candidate> {
        let mut out = Vec::with_capacity(hits.len());
        for h in hits {
            out.push(Candidate {
                key: h.chunk.id,
                score: h.score,
                channel,
            });
            hit_by_chunk.entry(h.chunk.id).or_insert(h);
        }
        *counts.entry(channel.name()).or_insert(0) += out.len();
        out
    };

    // Literal identifiers first, over the whole repo: files that contain an
    // identifier the question names join the routed scope of the other channels.
    let terms = literal_terms(question);
    let mut literal_hits: Vec<Hit> = Vec::new();
    if !terms.is_empty() {
        let per_term = (LITERAL_TOTAL / terms.len()).max(2) as i64;
        for term in &terms {
            literal_hits.extend(
                deps.search
                    .literal(repo.id, term, None, per_term)
                    .await
                    .with_context(|| format!("literal search for {term}"))?,
            );
        }
        literal_hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.chunk.id.cmp(&b.chunk.id))
        });
        literal_hits.dedup_by(|a, b| a.chunk.id == b.chunk.id);
        literal_hits.truncate(LITERAL_TOTAL);
    }
    let scope: Option<Vec<Uuid>> = file_ids.map(|routed| {
        let mut ids: Vec<Uuid> = routed
            .iter()
            .copied()
            .chain(literal_hits.iter().filter_map(|h| h.chunk.file_id))
            .collect();
        ids.sort();
        ids.dedup();
        ids
    });
    let literal_files = match (&scope, file_ids) {
        (Some(s), Some(r)) => s.len() - r.len(),
        _ => 0,
    };
    // The GraphQL anchor's files join the scope too.
    let scope = scope.map(|mut ids| {
        ids.extend_from_slice(entry_files);
        ids.sort();
        ids.dedup();
        ids
    });

    let semantic = deps
        .search
        .semantic(repo.id, &model, embedding, scope.as_deref(), SEMANTIC_LIMIT)
        .await
        .context("semantic search")?;
    let semantic = to_candidates(semantic, Channel::Semantic);
    let keyword = deps
        .search
        .keyword(repo.id, question, scope.as_deref(), KEYWORD_LIMIT)
        .await
        .context("keyword search")?;
    let keyword = to_candidates(keyword, Channel::Keyword);
    let literal = to_candidates(literal_hits, Channel::Literal);

    Ok(Candidates {
        hit_by_chunk,
        channels: vec![semantic, keyword, literal],
        counts,
        literal_files,
    })
}

/// Answer a feature question. See the module docs for the pipeline.
pub async fn feature(
    deps: Deps<'_>,
    repo: &Repo,
    root: &Path,
    req: FeatureRequest,
) -> Result<FeatureAnswer> {
    let budget_tokens = req.budget_tokens.clamp(MIN_BUDGET, MAX_BUDGET);
    let question = req.question.trim().to_string();
    anyhow::ensure!(!question.is_empty(), "feature: the question is empty");
    let model = deps.embedder.model_ref();
    let mut provenance = Vec::new();
    let mut warnings = Vec::new();

    tracing::info!(repo = %repo.name, budget_tokens, "feature: embedding question");
    let embedding = deps
        .embedder
        .embed(&question)
        .await
        .context("embedding the question")?;
    provenance.push(Breadcrumb::new(
        source::EMBEDDING,
        "embed",
        format!(
            "question embedded once with {}/{} ({} dims)",
            model.provider, model.model_id, model.dimensions
        ),
    ));

    // --- L1 routing -------------------------------------------------------
    let scored = deps
        .search
        .communities_by_embedding(repo.id, &model, &embedding, COMMUNITY_CANDIDATES)
        .await
        .context("community routing")?;
    let mut features: Vec<FeatureHit> = Vec::new();
    let mut file_ids: Option<Vec<Uuid>> = None;
    if scored.is_empty() {
        let any = deps
            .communities
            .load_communities(repo.id)
            .await
            .context("loading communities")?;
        let reason = if any.is_empty() {
            "repository has no feature communities (indexed without hierarchy)".to_string()
        } else {
            format!(
                "{} communities exist but none has a summary embedding for {}/{}",
                any.len(),
                model.provider,
                model.model_id
            )
        };
        warnings.push(format!("{reason}; falling back to whole-repo search"));
        provenance
            .push(Breadcrumb::new(source::POSTGRES, "route", reason).with_locator("communities"));
    } else {
        let pairs: Vec<(Uuid, f32)> = scored.iter().map(|(c, s)| (c.id, *s)).collect();
        let routed = fusion::route_communities(&pairs, ROUTE_FLOOR, ROUTE_MIN, ROUTE_MAX);
        let by_id: HashMap<Uuid, &(crate::domain::model::Community, f32)> =
            scored.iter().map(|p| (p.0.id, p)).collect();
        for id in &routed {
            let (c, s) = by_id[id];
            features.push(FeatureHit {
                id: c.id,
                label: c.label.clone(),
                score: *s,
                summary: truncate_chars(c.summary.as_deref().unwrap_or(""), SUMMARY_MAX),
                files: c.files.clone(),
            });
        }
        let ids = deps
            .search
            .community_file_ids(&routed)
            .await
            .context("community file ids")?;
        let mut ids = ids;
        ids.sort();
        ids.dedup();
        provenance.push(
            Breadcrumb::new(
                source::EMBEDDING,
                "cosine",
                format!("routed to {} of {} candidate communities (floor {ROUTE_FLOOR}, min {ROUTE_MIN}, max {ROUTE_MAX}) covering {} files", routed.len(), scored.len(), ids.len()),
            )
            .with_locator("communities.summary_embedding"),
        );
        if ids.is_empty() {
            warnings.push("routed communities own no files; searching the whole repo".into());
        } else {
            file_ids = Some(ids);
        }
    }

    // --- GraphQL entry anchor ----------------------------------------------
    let coords = graphql_entry::coordinates(&question, req.entry.as_deref());
    let (mut entry, entry_hits) = match graphql_entry::trace(deps.search, repo, &coords).await? {
        Some((t, hits)) => (Some(t), hits),
        None => (None, Vec::new()),
    };
    // The anchor is charged first, in full, trimmed to its share of the budget.
    let entry_cap =
        (budget_tokens as f64 * budget::CHARS_PER_TOKEN as f64 * ENTRY_BUDGET_SHARE) as usize;
    let mut entry_chars = 0;
    if let Some(t) = entry.as_mut() {
        let before = (t.trace.len(), t.sdl.len(), t.callers.len());
        entry_chars = t.fit(entry_cap);
        let dropped = [
            (before.0 - t.trace.len(), "call edges"),
            (before.1 - t.sdl.len(), "SDL copies"),
            (before.2 - t.callers.len(), "callers"),
        ]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect::<Vec<_>>();
        let mut detail = t.describe();
        if !dropped.is_empty() {
            detail.push_str(&format!(
                "; {} dropped to fit {entry_cap} chars",
                dropped.join(", ")
            ));
        }
        if entry_chars > entry_cap {
            detail.push_str(&format!(
                "; {entry_chars} chars exceed the {entry_cap}-char share (counted in tokens_used)"
            ));
        }
        provenance.push(
            Breadcrumb::new(source::GRAPH, "entry", detail)
                .with_locator("nodes.metadata.qualified_name"),
        );
    } else if let Some(asked) = req
        .entry
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        // An explicit `entry` that anchors nothing is said so, not dropped silently.
        provenance.push(
            Breadcrumb::new(
                source::GRAPH,
                "entry",
                format!("{asked}: no SDL field or calling operation in this repository; answer not anchored"),
            )
            .with_locator("nodes.metadata.qualified_name"),
        );
    }
    let entry_files: Vec<Uuid> = entry_hits.iter().filter_map(|h| h.chunk.file_id).collect();

    // --- fused retrieval ---------------------------------------------------
    let cands = retrieve(
        &deps,
        repo,
        &question,
        &embedding,
        file_ids.as_deref(),
        &entry_files,
    )
    .await?;
    let fused = fusion::fuse(cands.channels, FUSE_LIMIT);
    provenance.push(
        Breadcrumb::new(
            source::POSTGRES,
            "hybrid",
            format!(
                "semantic {} / keyword {} / literal {} hits fused to {} (weights 1.0/0.6/0.4, scope: {}; literal: whole repo)",
                cands.counts.get("semantic").copied().unwrap_or(0),
                cands.counts.get("keyword").copied().unwrap_or(0),
                cands.counts.get("literal").copied().unwrap_or(0),
                fused.len(),
                file_ids.as_ref().map(|f| format!("{} routed + {} literal-hit files{}", f.len(), cands.literal_files, match entry_files.iter().collect::<HashSet<_>>().len() {
                    0 => String::new(),
                    n => format!(" + {n} GraphQL entry files"),
                })).unwrap_or_else(|| "whole repo".into())
            ),
        )
        .with_locator("chunks"),
    );

    // --- 1-hop expansion ---------------------------------------------------
    let hit_by_chunk = cands.hit_by_chunk;
    let seed_nodes: Vec<Uuid> = {
        let mut v: Vec<Uuid> = fused
            .iter()
            .take(EXPAND_TOP)
            .filter_map(|f| hit_by_chunk.get(&f.key).and_then(|h| h.chunk.node_id))
            .collect();
        v.sort();
        v.dedup();
        v
    };
    let mut edges: Vec<Edge> = if seed_nodes.is_empty() {
        Vec::new()
    } else {
        deps.search
            .edges_for_nodes(repo.id, &seed_nodes)
            .await
            .context("edges for nodes")?
    };
    edges.retain(|e| e.kind != EdgeKind::Contains);
    edges.sort_by(|a, b| {
        (a.kind.as_str(), a.source_node_id, a.target_node_id).cmp(&(
            b.kind.as_str(),
            b.source_node_id,
            b.target_node_id,
        ))
    });
    edges.dedup_by(|a, b| {
        a.kind == b.kind
            && a.source_node_id == b.source_node_id
            && a.target_node_id == b.target_node_id
    });
    let seed_set: HashSet<Uuid> = seed_nodes.iter().copied().collect();
    let mut node_ids: BTreeSet<Uuid> = seed_set.iter().copied().collect();
    for e in &edges {
        node_ids.insert(e.source_node_id);
        node_ids.insert(e.target_node_id);
    }
    // Nodes of every fused excerpt and anchor chunk too (for symbol names).
    for f in &fused {
        if let Some(n) = hit_by_chunk.get(&f.key).and_then(|h| h.chunk.node_id) {
            node_ids.insert(n);
        }
    }
    node_ids.extend(entry_hits.iter().filter_map(|h| h.chunk.node_id));
    let node_id_list: Vec<Uuid> = node_ids.iter().copied().collect();
    let nodes = if node_id_list.is_empty() {
        Vec::new()
    } else {
        deps.search
            .nodes_by_ids(&node_id_list)
            .await
            .context("nodes by ids")?
    };
    let node_by_id: HashMap<Uuid, Node> = nodes.into_iter().map(|n| (n.id, n)).collect();
    let others: Vec<Uuid> = {
        let mut v: Vec<Uuid> = edges
            .iter()
            .flat_map(|e| [e.source_node_id, e.target_node_id])
            .filter(|n| !seed_set.contains(n))
            .collect();
        v.sort();
        v.dedup();
        v
    };
    let entry_chunk_ids: HashSet<Uuid> = entry_hits.iter().map(|h| h.chunk.id).collect();
    let fused_chunk_ids: HashSet<Uuid> = fused
        .iter()
        .map(|f| f.key)
        .chain(entry_chunk_ids.iter().copied())
        .collect();
    let mut expansion: Vec<Chunk> = if others.is_empty() {
        Vec::new()
    } else {
        deps.search
            .chunks_for_nodes(&others)
            .await
            .context("chunks for nodes")?
    };
    expansion.retain(|c| !fused_chunk_ids.contains(&c.id));
    expansion.sort_by(|a, b| a.id.cmp(&b.id));
    expansion.dedup_by(|a, b| a.id == b.id);
    // Keep the expansion in edge order (deterministic), capped.
    let rank: HashMap<Uuid, usize> = others.iter().enumerate().map(|(i, n)| (*n, i)).collect();
    expansion.sort_by_key(|c| {
        (
            c.node_id
                .and_then(|n| rank.get(&n).copied())
                .unwrap_or(usize::MAX),
            c.id,
        )
    });
    expansion.truncate(EXPAND_MAX_CHUNKS);
    let mut graph: Vec<GraphEdge> = edges
        .iter()
        .filter_map(|e| {
            let from = node_by_id.get(&e.source_node_id)?;
            let to = node_by_id.get(&e.target_node_id)?;
            Some(GraphEdge {
                from: node_symbol(from),
                kind: e.kind.as_str().to_string(),
                to: node_symbol(to),
                from_path: node_path(from),
                to_path: node_path(to),
            })
        })
        .collect();
    graph.sort();
    graph.dedup();
    // The anchor's binds / resolves / call-trace edges are emitted once, in `entry`.
    let entry_graph: BTreeSet<GraphEdge> = entry
        .as_ref()
        .map(EntryTrace::graph_edges)
        .unwrap_or_default()
        .into_iter()
        .collect();
    graph.retain(|g| !entry_graph.contains(g));
    graph.truncate(GRAPH_MAX_EDGES);
    provenance.push(
        Breadcrumb::new(
            source::GRAPH,
            "one-hop",
            format!(
                "{} edges around the top {} chunks, {} neighbour chunks appended",
                graph.len(),
                seed_nodes.len(),
                expansion.len()
            ),
        )
        .with_locator("edges"),
    );

    // --- paths for everything ---------------------------------------------
    let mut wanted_files: BTreeSet<Uuid> = expansion.iter().filter_map(|c| c.file_id).collect();
    let path_by_file: HashMap<Uuid, String> = {
        let mut map: HashMap<Uuid, String> = HashMap::new();
        for h in hit_by_chunk.values() {
            if let Some(f) = h.chunk.file_id {
                map.insert(f, h.path.clone());
            }
        }
        wanted_files.retain(|f| !map.contains_key(f));
        map
    };

    // --- excerpt candidates (fused order, then expansion) ------------------
    struct Raw {
        chunk: Chunk,
        path: Option<String>,
        score: f32,
        why: Vec<&'static str>,
    }
    let min_fused = fused.iter().map(|f| f.score).fold(f32::INFINITY, f32::min);
    let graph_score = if min_fused.is_finite() {
        min_fused * 0.5
    } else {
        0.0
    };
    // The anchor's chunks lead, scored at the top fused score; a chunk also
    // fused keeps its channel names after `entry`.
    let top_fused = fused
        .iter()
        .map(|f| f.score)
        .fold(f32::NEG_INFINITY, f32::max);
    let top_fused = if top_fused.is_finite() {
        top_fused
    } else {
        1.0
    };
    let mut raws: Vec<Raw> = entry_hits
        .iter()
        .map(|h| {
            let mut why = vec!["entry"];
            if let Some(f) = fused.iter().find(|f| f.key == h.chunk.id) {
                why.extend(f.channels.iter().map(|c| c.name()));
            }
            Raw {
                chunk: h.chunk.clone(),
                path: Some(h.path.clone()),
                score: top_fused,
                why,
            }
        })
        .collect();
    raws.extend(
        fused
            .iter()
            .filter(|f| !entry_chunk_ids.contains(&f.key))
            .filter_map(|f| {
                let h = hit_by_chunk.get(&f.key)?;
                Some(Raw {
                    chunk: h.chunk.clone(),
                    path: Some(h.path.clone()),
                    score: f.score,
                    why: f.channels.iter().map(|c| c.name()).collect(),
                })
            }),
    );
    for c in expansion {
        let path = c.file_id.and_then(|f| path_by_file.get(&f).cloned());
        raws.push(Raw {
            chunk: c,
            path,
            score: graph_score,
            why: vec!["graph"],
        });
    }

    // --- surfaces ----------------------------------------------------------
    let excerpt_files: BTreeSet<Uuid> = raws.iter().filter_map(|r| r.chunk.file_id).collect();
    let surface_scope: Vec<Uuid> = excerpt_files.iter().copied().collect();
    let mut surface_nodes = if surface_scope.is_empty() {
        Vec::new()
    } else {
        deps.search
            .surfaces(
                repo.id,
                &[NodeKind::EnvVar, NodeKind::HttpRoute, NodeKind::McpTool],
                Some(&surface_scope),
            )
            .await
            .context("surfaces")?
    };
    surface_nodes.retain(|n| n.file_id.is_some_and(|f| excerpt_files.contains(&f)));
    for n in &surface_nodes {
        if let Some(f) = n.file_id {
            if !path_by_file.contains_key(&f) {
                wanted_files.insert(f);
            }
        }
    }
    let mut path_by_file = path_by_file;
    if !wanted_files.is_empty() {
        let ids: Vec<Uuid> = wanted_files.iter().copied().collect();
        for (id, p) in deps.search.file_paths(&ids).await.context("file paths")? {
            path_by_file.insert(id, p);
        }
    }
    let mut surfaces: Vec<SurfaceHit> = surface_nodes
        .iter()
        .map(|n| SurfaceHit {
            kind: n.kind.as_str().to_string(),
            name: n.name.clone(),
            path: n
                .file_id
                .and_then(|f| path_by_file.get(&f).cloned())
                .unwrap_or_default(),
            line: n.line_start.unwrap_or(0),
        })
        .collect();
    surfaces.sort();
    surfaces.dedup();

    // --- budget ------------------------------------------------------------
    let summary_sizes: Vec<usize> = features.iter().map(|f| f.summary.len()).collect();
    let items: Vec<Item> = raws
        .iter()
        .map(|r| Item {
            header_chars: r.chunk.header.len(),
            body_chars: r.chunk.body.len() + 1,
        })
        .collect();
    let graph_chars: usize = graph.iter().map(edge_cost).sum::<usize>()
        + surfaces
            .iter()
            .map(|s| s.kind.len() + s.name.len() + s.path.len() + 36)
            .sum::<usize>();
    // The anchor (already fitted) is charged in full; the rest packs into
    // what its share left. An anchor whose irreducible rows overrun the share
    // takes no more from the rest: the overrun shows in `tokens_used`.
    let entry_tokens = entry_chars.div_ceil(budget::CHARS_PER_TOKEN);
    let reserved = entry_chars.min(entry_cap).div_ceil(budget::CHARS_PER_TOKEN);
    let packed = budget::pack(
        budget_tokens.saturating_sub(reserved),
        &summary_sizes,
        &items,
        graph_chars,
    );
    let kept: HashSet<usize> = packed.summaries.iter().copied().collect();
    for (i, f) in features.iter_mut().enumerate() {
        if !kept.contains(&i) {
            f.summary.clear();
        }
    }
    let mut excerpts = Vec::with_capacity(packed.excerpts.len());
    for (i, mode) in &packed.excerpts {
        let r = &raws[*i];
        let (code, truncated) = match mode {
            ExcerptMode::Full => (format!("{}\n{}", r.chunk.header, r.chunk.body), false),
            ExcerptMode::Clipped(n) => (
                format!(
                    "{}\n{}",
                    r.chunk.header,
                    budget::clip_at_line(&r.chunk.body, *n)
                ),
                true,
            ),
            ExcerptMode::HeaderOnly => (r.chunk.header.clone(), true),
        };
        let node = r.chunk.node_id.and_then(|n| node_by_id.get(&n));
        let path = r
            .path
            .clone()
            .or_else(|| r.chunk.file_id.and_then(|f| path_by_file.get(&f).cloned()))
            .unwrap_or_default();
        excerpts.push(Excerpt {
            symbol: node
                .map(node_symbol)
                .unwrap_or_else(|| header_symbol(&r.chunk, &path)),
            kind: node
                .map(|n| n.kind.as_str().to_string())
                .unwrap_or_else(|| r.chunk.kind.as_str().to_string()),
            path,
            lines: [
                r.chunk.line_start.unwrap_or(0),
                r.chunk.line_end.unwrap_or(0),
            ],
            score: r.score,
            why: r.why.clone(),
            code,
            code_truncated: truncated,
        });
    }
    // Graph + surfaces share one grant: trim from the tail while over it.
    let mut granted = packed.graph_chars;
    let mut kept_graph = Vec::new();
    for g in graph {
        let cost = edge_cost(&g);
        if cost <= granted {
            granted -= cost;
            kept_graph.push(g);
        } else {
            break;
        }
    }
    let mut kept_surfaces = Vec::new();
    for s in surfaces {
        let cost = s.kind.len() + s.name.len() + s.path.len() + 36;
        if cost <= granted {
            granted -= cost;
            kept_surfaces.push(s);
        } else {
            break;
        }
    }
    provenance.push(Breadcrumb::new(
        source::GRAPH,
        "budget",
        format!(
            "packed {} summaries, {} of {} excerpts, {} edges, {} surfaces{} under {} tokens",
            packed.summaries.len(),
            excerpts.len(),
            raws.len(),
            kept_graph.len(),
            kept_surfaces.len(),
            if entry.is_some() {
                format!(", the GraphQL entry ({entry_tokens} tokens)")
            } else {
                String::new()
            },
            budget_tokens
        ),
    ));

    // --- content hash + persistence ---------------------------------------
    let mut hashes: Vec<&str> = raws.iter().map(|r| r.chunk.content_hash.as_str()).collect();
    hashes.sort();
    hashes.dedup();
    // The anchor's registrations and callers are not chunks: fold them in.
    let entry_key = entry
        .as_ref()
        .and_then(|t| serde_json::to_string(t).ok())
        .map(|j| format!("\nentry:{}", hash::sha256_hex(&j)))
        .unwrap_or_default();
    let content_hash = hash::sha256_hex(&format!("{question}\n{}{entry_key}", hashes.join("\n")));

    let mut answer = FeatureAnswer {
        question: question.clone(),
        repo: repo.name.clone(),
        features,
        excerpts,
        graph: kept_graph,
        surfaces: kept_surfaces,
        entry,
        page: None,
        page_cached: false,
        content_hash,
        tokens_used: packed.tokens_used + entry_tokens,
        warnings,
        provenance,
    };

    if req.persist {
        let rel = PathBuf::from(FEATURES_DIR).join(format!("{}.html", slug(&question)));
        let abs = root.join(&rel);
        let manifest = manifest(&answer);
        let written = deps
            .pages
            .write_page(&abs, PageKind::Feature, &manifest)
            .with_context(|| format!("writing {}", abs.display()))?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        answer.provenance.push(
            Breadcrumb::new(
                source::PAGE,
                if written { "write" } else { "cached" },
                format!(
                    "feature page {}",
                    if written {
                        "written"
                    } else {
                        "unchanged (content hash matched)"
                    }
                ),
            )
            .with_locator(rel_str.clone()),
        );
        answer.page = Some(rel_str);
        answer.page_cached = !written;
        tracing::info!(page = %abs.display(), written, "feature: page");
    }
    Ok(answer)
}

/// The page manifest: the answer as JSON plus the aliases the Feature
/// template reads (`edges`, `start_line`/`end_line`, `retrieved_by`, `title`).
pub fn manifest(answer: &FeatureAnswer) -> Value {
    let mut v = serde_json::to_value(answer).unwrap_or_else(|_| json!({}));
    v["title"] = json!(answer.question);
    // The page's feature map draws the anchor's edges ahead of the 1-hop graph.
    let edges: Vec<GraphEdge> = answer
        .entry
        .as_ref()
        .map(EntryTrace::graph_edges)
        .unwrap_or_default()
        .into_iter()
        .chain(answer.graph.iter().cloned())
        .collect();
    v["edges"] = json!(edges);
    v["features"] = json!(answer
        .features
        .iter()
        .map(|f| json!({"id": f.id, "label": f.label, "score": f.score, "summary": f.summary, "files": f.files, "symbols": []}))
        .collect::<Vec<_>>());
    v["excerpts"] = json!(answer
        .excerpts
        .iter()
        .map(|e| {
            json!({
                "path": e.path, "lines": e.lines, "start_line": e.lines[0], "end_line": e.lines[1],
                "symbol": e.symbol, "kind": e.kind, "score": e.score, "why": e.why,
                "retrieved_by": e.why.join("+"), "code": e.code, "code_truncated": e.code_truncated,
            })
        })
        .collect::<Vec<_>>());
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{ChunkKind, Community, ModelRef};
    use async_trait::async_trait;
    use std::sync::Mutex;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn chunk(n: u128, node: Option<u128>, file: u128, body_len: usize) -> Chunk {
        Chunk {
            id: id(n),
            repo_id: id(1),
            file_id: Some(id(file)),
            node_id: node.map(id),
            kind: ChunkKind::Unit,
            header: format!("// file: src/f{file}.rs (rust)  L1-9\n// kind: function  name: fn{n}"),
            body: "x"
                .repeat(body_len)
                .chars()
                .enumerate()
                .map(|(i, c)| if i % 50 == 49 { '\n' } else { c })
                .collect(),
            content_hash: format!("hash{n}"),
            line_start: Some(1),
            line_end: Some(9),
            metadata: json!({}),
        }
    }

    fn hit(c: Chunk, score: f32) -> Hit {
        let path = format!("src/f{}.rs", c.file_id.unwrap().as_u128());
        Hit {
            chunk: c,
            path,
            score,
        }
    }

    fn node(n: u128, file: u128, kind: NodeKind, qualified: &str) -> Node {
        Node {
            id: id(n),
            repo_id: id(1),
            file_id: Some(id(file)),
            kind,
            stable_id: format!("rust:src/f{file}.rs:{}:{qualified}", kind.as_str()),
            name: qualified.rsplit("::").next().unwrap().to_string(),
            signature: None,
            doc: None,
            line_start: Some(3),
            line_end: Some(8),
            metadata: json!({}),
        }
    }

    fn community(n: u128, label: &str, summary: &str) -> Community {
        Community {
            id: id(n),
            repo_id: id(1),
            label: label.into(),
            member_count: 5,
            summary: Some(summary.into()),
            summary_hash: Some("g".into()),
            top_members: vec![],
            files: vec![format!("src/f{n}.rs")],
        }
    }

    #[derive(Default)]
    struct Script {
        communities: Vec<(Community, f32)>,
        all_communities: Vec<Community>,
        file_ids: Vec<Uuid>,
        semantic: Vec<Hit>,
        keyword: Vec<Hit>,
        literal: Vec<Hit>,
        edges: Vec<Edge>,
        nodes: Vec<Node>,
        expansion: Vec<Chunk>,
        surfaces: Vec<Node>,
        paths: Vec<(Uuid, String)>,
        /// SDL `Field` copies and operations the GraphQL entry lookups serve.
        gql_fields: Vec<Node>,
        ops: Vec<Node>,
        calls: Mutex<Vec<String>>,
    }

    impl Script {
        fn log(&self, s: impl Into<String>) {
            self.calls.lock().unwrap().push(s.into());
        }
    }

    #[async_trait]
    impl SearchStore for Script {
        async fn semantic(
            &self,
            _r: Uuid,
            _m: &ModelRef,
            _q: &[f32],
            file_ids: Option<&[Uuid]>,
            _l: i64,
        ) -> Result<Vec<Hit>> {
            self.log(format!("semantic scope={:?}", file_ids.map(|f| f.len())));
            Ok(self.semantic.clone())
        }
        async fn keyword(
            &self,
            _r: Uuid,
            q: &str,
            _f: Option<&[Uuid]>,
            _l: i64,
        ) -> Result<Vec<Hit>> {
            self.log(format!("keyword {q}"));
            Ok(self.keyword.clone())
        }
        async fn literal(
            &self,
            _r: Uuid,
            term: &str,
            file_ids: Option<&[Uuid]>,
            limit: i64,
        ) -> Result<Vec<Hit>> {
            self.log(format!(
                "literal {term} limit={limit} scope={:?}",
                file_ids.map(|f| f.len())
            ));
            Ok(self.literal.clone())
        }
        async fn communities_by_embedding(
            &self,
            _r: Uuid,
            _m: &ModelRef,
            _q: &[f32],
            _l: i64,
        ) -> Result<Vec<(Community, f32)>> {
            Ok(self.communities.clone())
        }
        async fn community_file_ids(&self, ids: &[Uuid]) -> Result<Vec<Uuid>> {
            self.log(format!("community_file_ids {}", ids.len()));
            Ok(self.file_ids.clone())
        }
        async fn edges_for_nodes(&self, _r: Uuid, ids: &[Uuid]) -> Result<Vec<Edge>> {
            Ok(self
                .edges
                .iter()
                .filter(|e| ids.contains(&e.source_node_id) || ids.contains(&e.target_node_id))
                .cloned()
                .collect())
        }
        async fn nodes_by_ids(&self, ids: &[Uuid]) -> Result<Vec<Node>> {
            Ok(self
                .nodes
                .iter()
                .filter(|n| ids.contains(&n.id))
                .cloned()
                .collect())
        }
        async fn chunks_for_nodes(&self, ids: &[Uuid]) -> Result<Vec<Chunk>> {
            Ok(self
                .expansion
                .iter()
                .filter(|c| c.node_id.is_some_and(|n| ids.contains(&n)))
                .cloned()
                .collect())
        }
        async fn surfaces(
            &self,
            _r: Uuid,
            _k: &[NodeKind],
            _f: Option<&[Uuid]>,
        ) -> Result<Vec<Node>> {
            Ok(self.surfaces.clone())
        }
        async fn file_paths(&self, ids: &[Uuid]) -> Result<Vec<(Uuid, String)>> {
            Ok(self
                .paths
                .iter()
                .filter(|(i, _)| ids.contains(i))
                .cloned()
                .collect())
        }
        async fn graphql_fields(&self, _r: Uuid, coords: &[String]) -> Result<Vec<Node>> {
            self.log(format!("graphql_fields {}", coords.join(",")));
            Ok(self
                .gql_fields
                .iter()
                .filter(|n| {
                    n.metadata["qualified_name"]
                        .as_str()
                        .is_some_and(|q| coords.iter().any(|c| c == q))
                })
                .cloned()
                .collect())
        }
        async fn operations_selecting(&self, _r: Uuid, coords: &[String]) -> Result<Vec<Node>> {
            self.log(format!("operations_selecting {}", coords.join(",")));
            Ok(self
                .ops
                .iter()
                .filter(|n| {
                    n.metadata["root_fields"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|f| f.as_str().is_some_and(|f| coords.iter().any(|c| c == f)))
                })
                .cloned()
                .collect())
        }
    }

    #[async_trait]
    impl CommunityStore for Script {
        async fn replace_communities(
            &self,
            _r: Uuid,
            _c: &[Community],
            _m: &[(Uuid, Uuid)],
        ) -> Result<()> {
            unreachable!()
        }
        async fn load_communities(&self, _r: Uuid) -> Result<Vec<Community>> {
            Ok(self.all_communities.clone())
        }
        async fn load_members(&self, _r: Uuid) -> Result<Vec<(Uuid, Uuid)>> {
            unreachable!()
        }
        async fn communities_needing_summary(
            &self,
            _r: Uuid,
            _m: &ModelRef,
        ) -> Result<Vec<Community>> {
            unreachable!()
        }
        async fn save_summary_embedding(&self, _c: Uuid, _m: &ModelRef, _e: &[f32]) -> Result<()> {
            unreachable!()
        }
    }

    struct ScriptedEmbedder {
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl Embedder for ScriptedEmbedder {
        fn provider(&self) -> &'static str {
            "test"
        }
        fn model_id(&self) -> &str {
            "scripted"
        }
        fn dimensions(&self) -> usize {
            3
        }
        async fn embed(&self, _input: &str) -> Result<Vec<f32>> {
            *self.calls.lock().unwrap() += 1;
            Ok(vec![1.0, 0.0, 0.0])
        }
    }

    #[derive(Default)]
    struct MemPages {
        pages: Mutex<HashMap<PathBuf, Value>>,
        writes: Mutex<usize>,
    }

    impl PageStore for MemPages {
        fn write_page(&self, path: &Path, _kind: PageKind, manifest: &Value) -> Result<bool> {
            let mut pages = self.pages.lock().unwrap();
            if pages.get(path).and_then(|m| m.get("content_hash")) == manifest.get("content_hash") {
                return Ok(false);
            }
            pages.insert(path.to_path_buf(), manifest.clone());
            *self.writes.lock().unwrap() += 1;
            Ok(true)
        }
        fn read_manifest(&self, path: &Path) -> Option<Value> {
            self.pages.lock().unwrap().get(path).cloned()
        }
    }

    fn repo() -> Repo {
        Repo {
            id: id(1),
            name: "api".into(),
            root_path: "/r".into(),
            commit_sha: None,
            worktree_paths: Some(Vec::new()),
            listing_rules: None,
        }
    }

    async fn run(script: &Script, pages: &MemPages, req: FeatureRequest) -> FeatureAnswer {
        let emb = ScriptedEmbedder {
            calls: Mutex::new(0),
        };
        let deps = Deps {
            search: script,
            communities: script,
            embedder: &emb,
            pages,
        };
        let out = feature(deps, &repo(), Path::new("/r"), req).await.unwrap();
        assert_eq!(
            *emb.calls.lock().unwrap(),
            1,
            "question embedded exactly once"
        );
        out
    }

    #[tokio::test]
    async fn routing_applies_floor_and_minimum() {
        let script = Script {
            communities: vec![
                (community(10, "storage", "s1"), 0.9),
                (community(11, "cli", "s2"), 0.2),
                (community(12, "mcp", "s3"), 0.1),
            ],
            file_ids: vec![id(100), id(101)],
            ..Default::default()
        };
        let out = run(
            &script,
            &MemPages::default(),
            FeatureRequest {
                persist: false,
                ..FeatureRequest::new("how are embeddings reused")
            },
        )
        .await;
        // 0.9 passes the floor; 0.2 is kept only by min_keep=2; 0.1 is dropped.
        assert_eq!(
            out.features
                .iter()
                .map(|f| f.label.as_str())
                .collect::<Vec<_>>(),
            vec!["storage", "cli"]
        );
        assert!(script
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "semantic scope=Some(2)"));
        assert!(out.warnings.is_empty());
        assert!(out.page.is_none());
    }

    #[tokio::test]
    async fn no_hierarchy_falls_back_to_whole_repo() {
        let script = Script {
            semantic: vec![hit(chunk(20, None, 100, 100), 0.5)],
            ..Default::default()
        };
        let out = run(
            &script,
            &MemPages::default(),
            FeatureRequest {
                persist: false,
                ..FeatureRequest::new("anything")
            },
        )
        .await;
        assert!(out.features.is_empty());
        assert_eq!(out.warnings.len(), 1);
        assert!(out.warnings[0].contains("no feature communities"));
        assert!(script
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "semantic scope=None"));
        assert_eq!(out.excerpts.len(), 1);
        assert_eq!(out.excerpts[0].symbol, "fn20");
        assert_eq!(out.excerpts[0].kind, "unit");
    }

    #[tokio::test]
    async fn fusion_orders_multi_channel_first_then_graph() {
        let c20 = chunk(20, Some(200), 100, 100);
        let c21 = chunk(21, Some(201), 100, 100);
        let c22 = chunk(22, Some(202), 101, 100);
        let neighbour = chunk(30, Some(300), 102, 100);
        let script = Script {
            communities: vec![
                (community(10, "storage", "s1"), 0.9),
                (community(11, "cli", "s2"), 0.8),
            ],
            file_ids: vec![id(100), id(101)],
            semantic: vec![hit(c20.clone(), 0.9), hit(c21.clone(), 0.8)],
            keyword: vec![hit(c21.clone(), 3.0)],
            literal: vec![hit(c22.clone(), 0.75)],
            edges: vec![Edge {
                id: id(9),
                repo_id: id(1),
                source_node_id: id(201),
                target_node_id: id(300),
                kind: EdgeKind::Calls,
                confidence: 1.0,
            }],
            nodes: vec![
                node(200, 100, NodeKind::Function, "alpha"),
                node(201, 100, NodeKind::Method, "Store::replace"),
                node(202, 101, NodeKind::Function, "gamma"),
                node(300, 102, NodeKind::Function, "insert_chunk"),
            ],
            expansion: vec![neighbour],
            surfaces: vec![
                node(400, 102, NodeKind::EnvVar, "DATABASE_URL"),
                node(401, 105, NodeKind::EnvVar, "ELSEWHERE"),
            ],
            paths: vec![(id(102), "src/f102.rs".into())],
            ..Default::default()
        };
        let out = run(
            &script,
            &MemPages::default(),
            FeatureRequest {
                persist: false,
                ..FeatureRequest::new("replace_repo embeddings reuse")
            },
        )
        .await;
        let symbols: Vec<&str> = out.excerpts.iter().map(|e| e.symbol.as_str()).collect();
        // c21: semantic (0.8/0.9) + keyword bonus → 0.889+0.15 beats c20 (0.75) and c22 (0.3).
        assert_eq!(
            symbols,
            vec!["Store::replace", "alpha", "gamma", "insert_chunk"]
        );
        assert_eq!(out.excerpts[0].why, vec!["semantic", "keyword"]);
        assert_eq!(out.excerpts[3].why, vec!["graph"]);
        assert_eq!(out.excerpts[3].path, "src/f102.rs");
        assert!(out.excerpts[3].score < out.excerpts[2].score);
        assert_eq!(out.excerpts[0].kind, "method");
        assert_eq!(
            out.graph,
            vec![GraphEdge {
                from: "Store::replace".into(),
                kind: "calls".into(),
                to: "insert_chunk".into(),
                from_path: "src/f100.rs".into(),
                to_path: "src/f102.rs".into(),
            }]
        );
        // Surfaces are filtered to files of the returned excerpts.
        assert_eq!(
            out.surfaces,
            vec![SurfaceHit {
                kind: "env_var".into(),
                name: "DATABASE_URL".into(),
                path: "src/f102.rs".into(),
                line: 3
            }]
        );
        assert!(
            script
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c == "literal replace_repo limit=20 scope=None"),
            "literal identifiers search the whole repo, not the routed files"
        );
        assert!(script
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "semantic scope=Some(2)"));
        assert!(out.excerpts.iter().all(|e| !e.code_truncated));
        assert!(out.tokens_used > 0 && out.tokens_used <= 6000);
    }

    #[tokio::test]
    async fn literal_hit_files_join_the_routed_scope() {
        let routed = chunk(20, Some(200), 100, 100);
        let named = chunk(22, Some(202), 105, 100);
        let script = Script {
            communities: vec![(community(10, "storage", "s1"), 0.9)],
            file_ids: vec![id(100), id(101)],
            semantic: vec![hit(routed.clone(), 0.9)],
            literal: vec![hit(named.clone(), 0.75)],
            nodes: vec![
                node(200, 100, NodeKind::Function, "alpha"),
                node(202, 105, NodeKind::Function, "replace_repo"),
            ],
            ..Default::default()
        };
        run(
            &script,
            &MemPages::default(),
            FeatureRequest {
                persist: false,
                ..FeatureRequest::new("where is replace_repo")
            },
        )
        .await;
        let calls = script.calls.lock().unwrap().clone();
        let at = |p: &str| calls.iter().position(|c| c.starts_with(p)).unwrap();
        assert!(at("literal replace_repo") < at("semantic"), "{calls:?}");
        assert!(
            calls.contains(&"semantic scope=Some(3)".to_string()),
            "{calls:?}"
        );
    }

    /// A node with an explicit stable id and metadata (GraphQL fixtures).
    fn gnode(n: u128, file: u128, kind: NodeKind, stable_id: &str, metadata: Value) -> Node {
        Node {
            id: id(n),
            repo_id: id(1),
            file_id: Some(id(file)),
            kind,
            stable_id: stable_id.into(),
            name: stable_id.rsplit([':', '.']).next().unwrap().to_string(),
            signature: None,
            doc: None,
            line_start: Some(13),
            line_end: Some(20),
            metadata,
        }
    }

    fn edge(n: u128, from: u128, to: u128, kind: EdgeKind, confidence: f32) -> Edge {
        Edge {
            id: id(n),
            repo_id: id(1),
            source_node_id: id(from),
            target_node_id: id(to),
            kind,
            confidence,
        }
    }

    /// desci-infra's shape: `getResolvers()` binds `Mutation.createLab` to the
    /// Labs Lambda, whose `switch (fieldName)` case calls
    /// `DataRoomResolvers::handleCreateLab`; a web hook and a test select it.
    fn create_lab_script() -> Script {
        let field = gnode(
            500,
            110,
            NodeKind::Field,
            "graphql:src/appsync/labs.graphql:field:Mutation.createLab",
            json!({"language": "graphql", "qualified_name": "Mutation.createLab",
                   "path": "src/appsync/labs.graphql",
                   "header": "createLab(input: CreateLabInput!): Lab!"}),
        );
        let resolver = gnode(
            501,
            111,
            NodeKind::Resolver,
            "typescript:lambda/appsync-resolver-labs-lambda/index.ts:resolver:originalHandler::createLab",
            json!({"fields": ["createLab"], "dispatch": "fieldName"}),
        );
        let registrar = gnode(
            502,
            112,
            NodeKind::Method,
            "typescript:lib/labs/config.ts:method:AppSyncLabsLambdaConfig::getResolvers",
            json!({}),
        );
        let registrar_file = Node {
            id: hash::scoped_id(id(1), "file:lib/labs/config.ts"),
            kind: NodeKind::File,
            name: "lib/labs/config.ts".into(),
            metadata: json!({
                "graphql_bindings": [{"coordinate": "Mutation.createLab", "line": 21,
                    "owner": "typescript:lib/labs/config.ts:method:AppSyncLabsLambdaConfig::getResolvers"}],
                "lambda_entries": ["lambda/appsync-resolver-labs-lambda/index.ts"]
            }),
            ..gnode(
                503,
                112,
                NodeKind::File,
                "file:lib/labs/config.ts",
                json!({}),
            )
        };
        let callee = gnode(
            504,
            113,
            NodeKind::Method,
            "typescript:lambda/appsync-resolver-labs-lambda/resolvers.ts:method:DataRoomResolvers::handleCreateLab",
            json!({}),
        );
        let op = |n: u128, path: &str| {
            gnode(
                n,
                114,
                NodeKind::Operation,
                &format!("graphql:{path}:operation:CreateLab"),
                json!({"root_fields": ["Mutation.createLab"], "path": path}),
            )
        };
        Script {
            communities: vec![(community(10, "labs", "s1"), 0.9)],
            file_ids: vec![id(100)],
            semantic: vec![
                hit(chunk(20, Some(200), 100, 100), 0.9),
                hit(chunk(21, Some(501), 111, 100), 0.5),
            ],
            gql_fields: vec![field.clone()],
            ops: vec![
                op(505, "apps/web/hooks/useCreateLab.ts"),
                op(506, "apps/web/__tests__/createLab.test.ts"),
            ],
            edges: vec![
                edge(1, 502, 500, EdgeKind::Binds, 1.0),
                edge(2, 501, 500, EdgeKind::Resolves, 0.9),
                edge(3, 501, 504, EdgeKind::Calls, 1.0),
            ],
            nodes: vec![
                node(200, 100, NodeKind::Function, "alpha"),
                field,
                resolver,
                registrar,
                registrar_file,
                callee,
            ],
            expansion: vec![
                chunk(21, Some(501), 111, 100),
                chunk(22, Some(504), 113, 100),
            ],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn entry_anchor_leads_the_answer() {
        let script = create_lab_script();
        let out = run(
            &script,
            &MemPages::default(),
            FeatureRequest {
                persist: false,
                ..FeatureRequest::new("Mutation.createLab")
            },
        )
        .await;
        let entry = out.entry.as_ref().expect("anchored");
        assert_eq!(entry.coordinates, vec!["Mutation.createLab"]);
        assert_eq!(entry.sdl[0].path, "src/appsync/labs.graphql");
        assert_eq!(
            (
                entry.bindings[0].symbol.as_str(),
                entry.bindings[0].line,
                entry.bindings[0].lambda_entries.clone()
            ),
            (
                "AppSyncLabsLambdaConfig::getResolvers",
                21,
                vec!["lambda/appsync-resolver-labs-lambda/index.ts".to_string()]
            )
        );
        assert_eq!(entry.resolvers[0].symbol, "originalHandler::createLab");
        assert_eq!(entry.resolvers[0].lines, [13, 20]);
        assert_eq!(entry.trace[0].to, "DataRoomResolvers::handleCreateLab");
        assert_eq!(entry.trace[0].hop, 1);
        // The test file's operation is not a caller.
        assert_eq!(entry.callers.len(), 1);
        assert_eq!(entry.callers[0].path, "apps/web/hooks/useCreateLab.ts");

        // Anchor chunks lead: the resolver (also semantic), then its callee.
        let lead: Vec<(&str, Vec<&str>)> = out
            .excerpts
            .iter()
            .map(|e| (e.symbol.as_str(), e.why.clone()))
            .collect();
        assert_eq!(
            lead,
            vec![
                ("originalHandler::createLab", vec!["entry", "semantic"]),
                ("DataRoomResolvers::handleCreateLab", vec!["entry"]),
                ("alpha", vec!["semantic"]),
            ]
        );
        assert_eq!(
            out.excerpts[0].path,
            "lambda/appsync-resolver-labs-lambda/index.ts"
        );
        // The anchor's binds, resolves and call trace live in `entry` only:
        // the 1-hop graph around the resolver chunk does not repeat them.
        let anchor = entry.graph_edges();
        let kinds: Vec<&str> = anchor.iter().map(|g| g.kind.as_str()).collect();
        assert_eq!(kinds, vec!["binds", "resolves", "calls"]);
        assert_eq!(anchor[0].to_path, "src/appsync/labs.graphql");
        assert!(
            out.graph.iter().all(|g| !anchor.contains(g)),
            "{:?}",
            out.graph
        );
        // The page's feature map still draws them, ahead of the 1-hop graph.
        let page = manifest(&out);
        let drawn: Vec<&str> = page["edges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["kind"].as_str().unwrap())
            .collect();
        assert_eq!(drawn[..3], ["binds", "resolves", "calls"]);
        assert!(out
            .provenance
            .iter()
            .any(|b| b.method == "entry" && b.detail.contains("1 resolvers")));
        // The anchor's files join the retrieval scope (routed 1 + 2 entry files).
        let calls = script.calls.lock().unwrap().clone();
        assert!(
            calls.contains(&"semantic scope=Some(3)".to_string()),
            "{calls:?}"
        );
        // The JSON carries the anchor.
        assert!(serde_json::to_value(&out).unwrap()["entry"]["resolvers"].is_array());
    }

    /// `create_lab_script` behind a wide, deep call trace (8 more hop-1
    /// callees, 2 hop-2 callees each, 1 hop-3 callee each of those, long
    /// names), a second SDL copy of the field, as desci-infra's createLab.
    fn wide_trace_script() -> Script {
        let mut s = create_lab_script();
        let dir = "lambda/appsync-resolver-labs-lambda/services/data-room";
        let mut n = 600u128;
        let mut next = || {
            n += 1;
            n
        };
        let mut calls: Vec<(u128, u128)> = Vec::new();
        for i in 0..8 {
            let a = next();
            calls.push((501, a));
            s.nodes.push(gnode(a, 113, NodeKind::Method, &format!("typescript:{dir}/resolvers-{i}.ts:method:DataRoomResolverService{i}::handleCreateLabStep{i}"), json!({})));
            for j in 0..2 {
                let b = next();
                calls.push((a, b));
                s.nodes.push(gnode(b, 113, NodeKind::Function, &format!("typescript:{dir}/helpers-{i}-{j}.ts:function:validateAndPersistLabRecord{i}{j}"), json!({})));
                let c = next();
                calls.push((b, c));
                s.nodes.push(gnode(c, 113, NodeKind::Function, &format!("typescript:{dir}/aurora-{i}-{j}.ts:function:writeLabRowInsideTransaction{i}{j}"), json!({})));
            }
        }
        for (k, (from, to)) in calls.into_iter().enumerate() {
            s.edges
                .push(edge(100 + k as u128, from, to, EdgeKind::Calls, 0.9));
        }
        let copy = gnode(
            520,
            115,
            NodeKind::Field,
            "graphql:graphql/schemas/ip-hubs.graphql:field:Mutation.createLab",
            json!({"language": "graphql", "qualified_name": "Mutation.createLab",
                   "path": "graphql/schemas/ip-hubs.graphql",
                   "header": "createLab(input: CreateLabInput!): Lab!"}),
        );
        s.gql_fields.push(copy.clone());
        s.nodes.push(copy);
        s
    }

    #[tokio::test]
    async fn entry_anchor_is_charged_and_trimmed_to_its_share() {
        let script = wide_trace_script();
        let ask = |budget_tokens| FeatureRequest {
            budget_tokens,
            persist: false,
            ..FeatureRequest::new("Mutation.createLab")
        };
        let full = run(&script, &MemPages::default(), ask(MAX_BUDGET)).await;
        let full = full.entry.expect("anchored");
        assert_eq!(full.trace.len(), graphql_entry::ENTRY_MAX_TRACE_EDGES);
        assert_eq!(full.sdl.len(), 2);
        let hop1 = full.trace.iter().filter(|t| t.hop == 1).count();
        for budget_tokens in [MIN_BUDGET, DEFAULT_BUDGET, MAX_BUDGET] {
            let out = run(&script, &MemPages::default(), ask(budget_tokens)).await;
            let entry = out.entry.as_ref().expect("anchored");
            let total = budget_tokens * budget::CHARS_PER_TOKEN;
            assert!(
                entry.chars() <= total / 4,
                "{budget_tokens}: {}",
                entry.chars()
            );
            // Cut from the deepest hop; registrations and resolvers never.
            assert_eq!(entry.trace[..], full.trace[..entry.trace.len()]);
            assert_eq!(
                (&entry.bindings, &entry.resolvers),
                (&full.bindings, &full.resolvers)
            );
            assert_eq!(entry.sdl[0], full.sdl[0]);
            // Every emitted char is counted, and the count is within budget.
            let emitted = entry.chars()
                + out.graph.iter().map(edge_cost).sum::<usize>()
                + out.excerpts.iter().map(|e| e.code.len()).sum::<usize>()
                + out.features.iter().map(|f| f.summary.len()).sum::<usize>();
            assert!(
                emitted <= out.tokens_used * budget::CHARS_PER_TOKEN,
                "{budget_tokens}: {emitted} chars emitted, {} tokens reported",
                out.tokens_used
            );
            assert!(out.tokens_used <= budget_tokens, "{}", out.tokens_used);
            // The anchor's edges are not repeated in the graph.
            let anchor = entry.graph_edges();
            assert!(out.graph.iter().all(|g| !anchor.contains(g)));
        }
        // The default budget keeps hop 1 and drops deeper edges first.
        let mid = run(&script, &MemPages::default(), ask(DEFAULT_BUDGET)).await;
        let entry = mid.entry.as_ref().unwrap();
        assert!(entry.trace.len() < full.trace.len());
        assert_eq!(entry.trace.iter().filter(|t| t.hop == 1).count(), hop1);
        let dropped = format!(
            "{} call edges dropped to fit 6000 chars",
            full.trace.len() - entry.trace.len()
        );
        assert!(
            mid.provenance
                .iter()
                .any(|b| b.method == "entry" && b.detail.ends_with(&dropped)),
            "{:?}",
            mid.provenance
        );
        // The smallest budget: the SDL copy and the callers go before hop 1.
        let min = run(&script, &MemPages::default(), ask(MIN_BUDGET)).await;
        let entry = min.entry.unwrap();
        assert_eq!((entry.sdl.len(), entry.callers.len()), (1, 0));
        assert!(entry.trace.len() < hop1 && entry.trace.iter().all(|t| t.hop == 1));
        assert_eq!(full.callers.len(), 1);
    }

    #[tokio::test]
    async fn explicit_entry_argument_anchors_a_bare_name() {
        let script = create_lab_script();
        let out = run(
            &script,
            &MemPages::default(),
            FeatureRequest {
                persist: false,
                entry: Some("createLab".into()),
                ..FeatureRequest::new("how are labs made")
            },
        )
        .await;
        let entry = out.entry.expect("anchored by the argument");
        assert_eq!(entry.sdl[0].coordinate, "Mutation.createLab");
        // Query.createLab / Subscription.createLab were asked for, not found.
        assert_eq!(entry.coordinates, vec!["Mutation.createLab"]);
        assert_eq!(out.excerpts[0].why[0], "entry");
    }

    #[tokio::test]
    async fn explicit_entry_that_anchors_nothing_is_reported() {
        let script = create_lab_script();
        let out = run(
            &script,
            &MemPages::default(),
            FeatureRequest {
                persist: false,
                entry: Some("Mutation.deleteLab".into()),
                ..FeatureRequest::new("how are labs made")
            },
        )
        .await;
        assert!(out.entry.is_none());
        assert!(
            out.provenance.iter().any(|b| b.method == "entry"
                && b.detail.starts_with("Mutation.deleteLab: no SDL field")
                && b.detail.ends_with("answer not anchored")),
            "{:?}",
            out.provenance
        );
        // A malformed entry yields no coordinate at all and is reported too.
        let out = run(
            &script,
            &MemPages::default(),
            FeatureRequest {
                persist: false,
                entry: Some("create lab".into()),
                ..FeatureRequest::new("how are labs made")
            },
        )
        .await;
        assert!(out.entry.is_none());
        assert!(out
            .provenance
            .iter()
            .any(|b| b.method == "entry" && b.detail.starts_with("create lab: ")));
    }

    #[tokio::test]
    async fn unknown_coordinate_changes_nothing() {
        let anchored = create_lab_script();
        let plain = Script {
            gql_fields: vec![],
            ops: vec![],
            ..create_lab_script()
        };
        let ask = || FeatureRequest {
            persist: false,
            ..FeatureRequest::new("Mutation.deleteLab")
        };
        let a = run(&anchored, &MemPages::default(), ask()).await;
        let b = run(&plain, &MemPages::default(), ask()).await;
        assert!(a.entry.is_none() && b.entry.is_none());
        assert_eq!(a.excerpts, b.excerpts);
        assert_eq!(a.graph, b.graph);
        assert_eq!(a.content_hash, b.content_hash);
        assert!(a.excerpts.iter().all(|e| !e.why.contains(&"entry")));
        assert!(!serde_json::to_value(&a)
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("entry"));
        assert!(a.provenance.iter().all(|p| p.method != "entry"));
    }

    #[tokio::test]
    async fn bare_name_without_cue_word_is_not_an_entry() {
        let script = create_lab_script();
        let out = run(
            &script,
            &MemPages::default(),
            FeatureRequest {
                persist: false,
                ..FeatureRequest::new("how does createLab work")
            },
        )
        .await;
        assert!(out.entry.is_none());
        let calls = script.calls.lock().unwrap().clone();
        assert!(
            calls.iter().all(|c| !c.starts_with("graphql_fields")),
            "{calls:?}"
        );
        // With a cue word the same name anchors.
        let cued = run(
            &script,
            &MemPages::default(),
            FeatureRequest {
                persist: false,
                ..FeatureRequest::new("how does the createLab mutation work")
            },
        )
        .await;
        assert!(cued.entry.is_some());
    }

    #[tokio::test]
    async fn budget_degrades_to_header_only() {
        // 1000 tokens = 4000 chars → excerpts get 2800 + unused summary share.
        let big = chunk(20, Some(200), 100, 2500);
        let also_big = chunk(21, Some(201), 100, 2500);
        let script = Script {
            communities: vec![
                (community(10, "storage", &"s".repeat(100)), 0.9),
                (community(11, "cli", &"t".repeat(100)), 0.8),
            ],
            file_ids: vec![id(100)],
            semantic: vec![hit(big, 0.9), hit(also_big, 0.8)],
            nodes: vec![
                node(200, 100, NodeKind::Function, "alpha"),
                node(201, 100, NodeKind::Function, "beta"),
            ],
            ..Default::default()
        };
        let out = run(
            &script,
            &MemPages::default(),
            FeatureRequest {
                question: "alpha".into(),
                budget_tokens: 1000,
                persist: false,
                entry: None,
            },
        )
        .await;
        assert_eq!(out.excerpts.len(), 2);
        assert!(!out.excerpts[0].code_truncated);
        assert!(out.excerpts[1].code_truncated);
        assert_eq!(
            out.excerpts[1].code,
            out.excerpts[1]
                .code
                .lines()
                .take(2)
                .collect::<Vec<_>>()
                .join("\n"),
            "header only"
        );
        assert_eq!(
            out.features
                .iter()
                .filter(|f| !f.summary.is_empty())
                .count(),
            2
        );
        assert!(out.tokens_used <= 1000);
        // Budget below the minimum is clamped up, not rejected.
        let out = run(
            &script,
            &MemPages::default(),
            FeatureRequest {
                question: "alpha".into(),
                budget_tokens: 10,
                persist: false,
                entry: None,
            },
        )
        .await;
        assert!(out.tokens_used <= MIN_BUDGET);
    }

    #[tokio::test]
    async fn page_written_once_then_cached() {
        let script = Script {
            communities: vec![(community(10, "storage", "s1"), 0.9)],
            file_ids: vec![id(100)],
            semantic: vec![hit(chunk(20, Some(200), 100, 100), 0.9)],
            nodes: vec![node(200, 100, NodeKind::Function, "alpha")],
            ..Default::default()
        };
        let pages = MemPages::default();
        let first = run(
            &script,
            &pages,
            FeatureRequest::new("How are Embeddings reused?"),
        )
        .await;
        assert_eq!(
            first.page.as_deref(),
            Some("docs/features_memory/how-are-embeddings-reused.html")
        );
        assert!(!first.page_cached);
        let manifest = pages
            .read_manifest(Path::new(
                "/r/docs/features_memory/how-are-embeddings-reused.html",
            ))
            .unwrap();
        assert_eq!(manifest["content_hash"], json!(first.content_hash));
        assert_eq!(manifest["edges"], json!([]));
        assert_eq!(manifest["excerpts"][0]["start_line"], json!(1));
        assert_eq!(manifest["excerpts"][0]["retrieved_by"], json!("semantic"));
        assert_eq!(manifest["title"], json!("How are Embeddings reused?"));

        let second = run(
            &script,
            &pages,
            FeatureRequest::new("How are Embeddings reused?"),
        )
        .await;
        assert!(second.page_cached);
        assert_eq!(second.content_hash, first.content_hash);
        assert_eq!(*pages.writes.lock().unwrap(), 1);
        assert!(second
            .provenance
            .iter()
            .any(|b| b.source == source::PAGE && b.method == "cached"));

        // A different question hashes differently and writes a new page.
        let third = run(&script, &pages, FeatureRequest::new("Why reuse embeddings")).await;
        assert!(!third.page_cached);
        assert_ne!(third.content_hash, first.content_hash);
        assert_eq!(*pages.writes.lock().unwrap(), 2);
    }

    #[test]
    fn literal_terms_pick_identifiers() {
        assert_eq!(
            literal_terms("how does replace_repo reuse embeddings via content_hash?"),
            vec!["replace_repo", "content_hash"]
        );
        assert_eq!(
            literal_terms("where is ChunkSearch::semantic called from src/query.rs, fetchUser"),
            vec!["ChunkSearch::semantic", "src/query.rs", "fetchUser"]
        );
        assert!(literal_terms("plain english words only").is_empty());
        assert_eq!(
            literal_terms("a_b a_b"),
            vec!["a_b"]
                .into_iter()
                .filter(|_| false)
                .collect::<Vec<_>>(),
            "too short"
        );
    }

    #[test]
    fn slug_is_stable() {
        assert_eq!(
            slug("How are Embeddings reused?"),
            "how-are-embeddings-reused"
        );
        assert_eq!(slug("???"), "page");
        assert!(slug(&"x y ".repeat(100)).len() <= 80);
    }

    #[test]
    fn node_symbol_uses_qualified_tail() {
        assert_eq!(
            node_symbol(&node(1, 2, NodeKind::Method, "Store::replace")),
            "Store::replace"
        );
        let mut file = node(1, 2, NodeKind::File, "x");
        file.stable_id = "file:src/a.rs".into();
        assert_eq!(node_symbol(&file), "src/a.rs");
    }
}
