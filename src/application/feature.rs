//! Use-case `feature`: answer ONE feature question from the persisted index
//! without reading source files.
//!
//! Pipeline: embed the question once → route to the best-matching feature
//! communities by summary embedding → fused chunk retrieval (semantic /
//! keyword / literal) restricted to those communities' files → 1-hop graph
//! expansion of the top hits → env-var / HTTP-route surfaces of the involved
//! files → pack everything under a token budget → optionally persist the
//! answer as a hash-gated page. Deterministic given the store outputs.

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
}

impl FeatureRequest {
    pub fn new(question: impl Into<String>) -> Self {
        Self {
            question: question.into(),
            budget_tokens: DEFAULT_BUDGET,
            persist: true,
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
    /// `semantic` / `keyword` / `literal` / `graph`.
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

fn looks_like_identifier(tok: &str) -> bool {
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
fn node_symbol(node: &Node) -> String {
    let parts: Vec<&str> = node.stable_id.splitn(4, ':').collect();
    match parts.as_slice() {
        [_, _, _, tail] if !tail.is_empty() => tail.to_string(),
        ["file", path] => path.to_string(),
        _ => node.name.clone(),
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
}

async fn retrieve(
    deps: &Deps<'_>,
    repo: &Repo,
    question: &str,
    embedding: &[f32],
    file_ids: Option<&[Uuid]>,
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

    let semantic = deps
        .search
        .semantic(repo.id, &model, embedding, file_ids, SEMANTIC_LIMIT)
        .await
        .context("semantic search")?;
    let semantic = to_candidates(semantic, Channel::Semantic);
    let keyword = deps
        .search
        .keyword(repo.id, question, file_ids, KEYWORD_LIMIT)
        .await
        .context("keyword search")?;
    let keyword = to_candidates(keyword, Channel::Keyword);

    let terms = literal_terms(question);
    let mut literal_hits: Vec<Hit> = Vec::new();
    if !terms.is_empty() {
        let per_term = (LITERAL_TOTAL / terms.len()).max(2) as i64;
        for term in &terms {
            literal_hits.extend(
                deps.search
                    .literal(repo.id, term, file_ids, per_term)
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
    let literal = to_candidates(literal_hits, Channel::Literal);

    Ok(Candidates {
        hit_by_chunk,
        channels: vec![semantic, keyword, literal],
        counts,
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

    // --- fused retrieval ---------------------------------------------------
    let cands = retrieve(&deps, repo, &question, &embedding, file_ids.as_deref()).await?;
    let fused = fusion::fuse(cands.channels, FUSE_LIMIT);
    provenance.push(
        Breadcrumb::new(
            source::POSTGRES,
            "hybrid",
            format!(
                "semantic {} / keyword {} / literal {} hits fused to {} (weights 1.0/0.6/0.4, scope: {})",
                cands.counts.get("semantic").copied().unwrap_or(0),
                cands.counts.get("keyword").copied().unwrap_or(0),
                cands.counts.get("literal").copied().unwrap_or(0),
                fused.len(),
                file_ids.as_ref().map(|f| format!("{} routed files", f.len())).unwrap_or_else(|| "whole repo".into())
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
    // Nodes of every fused excerpt too (for symbol names).
    for f in &fused {
        if let Some(n) = hit_by_chunk.get(&f.key).and_then(|h| h.chunk.node_id) {
            node_ids.insert(n);
        }
    }
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
    let fused_chunk_ids: HashSet<Uuid> = fused.iter().map(|f| f.key).collect();
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
            })
        })
        .collect();
    graph.sort();
    graph.dedup();
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
    let mut raws: Vec<Raw> = fused
        .iter()
        .filter_map(|f| {
            let h = hit_by_chunk.get(&f.key)?;
            Some(Raw {
                chunk: h.chunk.clone(),
                path: Some(h.path.clone()),
                score: f.score,
                why: f.channels.iter().map(|c| c.name()).collect(),
            })
        })
        .collect();
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
    let surface_scope: Vec<Uuid> = match &file_ids {
        Some(ids) => ids.clone(),
        None => excerpt_files.iter().copied().collect(),
    };
    let mut surface_nodes = if surface_scope.is_empty() {
        Vec::new()
    } else {
        deps.search
            .surfaces(
                repo.id,
                &[NodeKind::EnvVar, NodeKind::HttpRoute],
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
    let graph_chars: usize = graph
        .iter()
        .map(|g| g.from.len() + g.kind.len() + g.to.len() + 24)
        .sum::<usize>()
        + surfaces
            .iter()
            .map(|s| s.kind.len() + s.name.len() + s.path.len() + 36)
            .sum::<usize>();
    let packed = budget::pack(budget_tokens, &summary_sizes, &items, graph_chars);
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
        let cost = g.from.len() + g.kind.len() + g.to.len() + 24;
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
            "packed {} summaries, {} of {} excerpts, {} edges, {} surfaces under {} tokens",
            packed.summaries.len(),
            excerpts.len(),
            raws.len(),
            kept_graph.len(),
            kept_surfaces.len(),
            budget_tokens
        ),
    ));

    // --- content hash + persistence ---------------------------------------
    let mut hashes: Vec<&str> = raws.iter().map(|r| r.chunk.content_hash.as_str()).collect();
    hashes.sort();
    hashes.dedup();
    let content_hash = hash::sha256_hex(&format!("{question}\n{}", hashes.join("\n")));

    let mut answer = FeatureAnswer {
        question: question.clone(),
        repo: repo.name.clone(),
        features,
        excerpts,
        graph: kept_graph,
        surfaces: kept_surfaces,
        page: None,
        page_cached: false,
        content_hash,
        tokens_used: packed.tokens_used,
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
    v["edges"] = json!(answer.graph);
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
            subtree_hash: "h".into(),
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
            _f: Option<&[Uuid]>,
            limit: i64,
        ) -> Result<Vec<Hit>> {
            self.log(format!("literal {term} limit={limit}"));
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
        async fn edges_for_nodes(&self, _r: Uuid, _n: &[Uuid]) -> Result<Vec<Edge>> {
            Ok(self.edges.clone())
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
        async fn save_summary(
            &self,
            _c: Uuid,
            _s: &str,
            _h: &str,
            _m: &ModelRef,
            _e: &[f32],
        ) -> Result<()> {
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
            root_hash: None,
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
                to: "insert_chunk".into()
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
        assert!(script
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("literal replace_repo")));
        assert!(out.excerpts.iter().all(|e| !e.code_truncated));
        assert!(out.tokens_used > 0 && out.tokens_used <= 6000);
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
