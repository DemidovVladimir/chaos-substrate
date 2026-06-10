use crate::{
    embedding::Embedder,
    graph::{best_context_paths, ContextPath},
    models::SearchHit,
    storage::Storage,
};
use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct QueryResponse {
    pub hits: Vec<SearchHit>,
    pub context_paths: Vec<ContextPath>,
}

/// A god-node (community) the query routed into — the top-down entry point.
#[derive(Debug, Serialize)]
pub struct CommunityRoute {
    pub id: Uuid,
    pub label: String,
    pub member_count: i32,
    pub score: f64,
    pub summary: Option<String>,
}

/// Hierarchical (top-down) query result: the matched features first, then the
/// chunk-level hits (boosted toward those features), then context paths.
#[derive(Debug, Serialize)]
pub struct HierarchicalResponse {
    /// `"hierarchical"` when communities matched, `"flat-fallback"` otherwise.
    pub mode: &'static str,
    pub communities: Vec<CommunityRoute>,
    pub hits: Vec<SearchHit>,
    pub context_paths: Vec<ContextPath>,
}

/// Cap on a hit's chunk content in TOOL RETURNS (the full text stays in the
/// index and in generated HTML; the agent can always read the file). Applied at
/// the MCP/CLI boundary, never inside retrieval — HTML evidence keeps full text.
pub const MAX_RETURN_CHUNK_CHARS: usize = 800;
/// Cap on a community summary inside a hierarchical route return.
pub const MAX_ROUTE_SUMMARY_CHARS: usize = 400;

/// Truncate text for a tool return at a char boundary, marking the cut.
pub fn truncate_for_return(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars).collect();
    let dropped = text.chars().count() - max_chars;
    format!("{kept}… [+{dropped} chars in the indexed chunk]")
}

/// Cap every hit's content for a tool return (see [`MAX_RETURN_CHUNK_CHARS`]).
pub fn cap_hits_for_return(hits: &mut [SearchHit]) {
    for hit in hits {
        hit.content = truncate_for_return(&hit.content, MAX_RETURN_CHUNK_CHARS);
    }
}

/// Minimum cosine score for a community to count as a matched feature.
const COMMUNITY_MATCH_FLOOR: f64 = 0.30;
/// Score assigned to a feature matched only by a label/token match (no cosine).
const LABEL_ROUTE_SCORE: f64 = 0.5;
/// Cap on label-only routes added as a fallback (keeps boosting focused).
const LABEL_ROUTE_LIMIT: usize = 5;
/// Minimum query-token length considered for the label-match fallback.
const MIN_ROUTE_TOKEN: usize = 3;

/// Top-down retrieval: match the query against community summary embeddings
/// first, then run the flat hybrid search and boost hits whose node lives in a
/// matched feature. Falls back to the flat path when no communities exist
/// (additivity — a repo indexed before the hierarchy still answers).
pub async fn query_repo_hierarchical(
    storage: &Storage,
    repo_id: Uuid,
    embedder: &dyn Embedder,
    query: &str,
    limit: i64,
) -> Result<HierarchicalResponse> {
    let query_embedding = embedder.embed(query).await?;
    let matches = storage
        .community_semantic_search(
            repo_id,
            embedder.provider(),
            embedder.model_id(),
            embedder.dimensions(),
            &query_embedding,
            8,
        )
        .await?;
    // Routes by cosine over the L3 summary embeddings (top-down semantic match).
    let mut route_map: HashMap<Uuid, CommunityRoute> = HashMap::new();
    for m in &matches {
        if m.score >= COMMUNITY_MATCH_FLOOR && m.member_count >= 2 {
            route_map.insert(
                m.id,
                CommunityRoute {
                    id: m.id,
                    label: m.label.clone(),
                    member_count: m.member_count,
                    score: m.score,
                    summary: m.summary.clone(),
                },
            );
        }
    }

    // Lexical label fallback: the extractive L3 summaries embed weakly, so a
    // path/label-named feature (e.g. "OCL") often never clears the cosine floor
    // and the router would silently drop to flat. Match the query's significant
    // tokens against community LABELS (path-derived) — the same rescue
    // `chaos_components` relies on. A TRUE fallback: it only runs when the
    // cosine pass routed nothing, so a fixed-score label route can never
    // outrank or dilute genuine semantic routes.
    let tokens = router_label_tokens(query);
    if route_map.is_empty() && !tokens.is_empty() {
        let labels = storage.community_labels(repo_id).await?;
        let mut lexical: Vec<(Uuid, String, i32)> = labels
            .into_iter()
            .filter(|(id, label, _)| {
                !route_map.contains_key(id) && label_matches_tokens(label, &tokens)
            })
            .collect();
        // Prefer larger (more central) features; bound the fallback.
        lexical.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        for (id, label, member_count) in lexical.into_iter().take(LABEL_ROUTE_LIMIT) {
            route_map.insert(
                id,
                CommunityRoute {
                    id,
                    label,
                    member_count,
                    score: LABEL_ROUTE_SCORE,
                    summary: None,
                },
            );
        }
    }

    let mut routes: Vec<CommunityRoute> = route_map.into_values().collect();
    // Deterministic order: strongest first, then larger features, then id.
    routes.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.member_count.cmp(&a.member_count))
            .then_with(|| a.id.cmp(&b.id))
    });
    // Routes are a return-only surface: trim each summary (full text lives in
    // the communities table and every generated feature page).
    for route in &mut routes {
        if let Some(summary) = &route.summary {
            route.summary = Some(truncate_for_return(summary, MAX_ROUTE_SUMMARY_CHARS));
        }
    }

    // Reuse the query embedding computed for community routing — the flat
    // search would otherwise embed the identical text a second time.
    let flat = query_repo_with_expansions(
        storage,
        repo_id,
        embedder,
        query,
        limit,
        &[query],
        Some(&query_embedding),
    )
    .await?;
    if routes.is_empty() {
        return Ok(HierarchicalResponse {
            mode: "flat-fallback",
            communities: Vec::new(),
            hits: flat.hits,
            context_paths: flat.context_paths,
        });
    }

    // Boost hits whose node belongs to a matched feature, then re-rank.
    let top_ids: std::collections::HashSet<Uuid> = routes.iter().map(|r| r.id).collect();
    let node_ids: Vec<Uuid> = flat.hits.iter().filter_map(|h| h.node_id).collect();
    let membership = storage.node_communities(repo_id, &node_ids).await?;
    let mut hits = flat.hits;
    for hit in &mut hits {
        if let Some(node_id) = hit.node_id {
            if membership
                .get(&node_id)
                .is_some_and(|comms| comms.iter().any(|c| top_ids.contains(c)))
            {
                hit.score *= 1.5;
            }
        }
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    // The hierarchical response is a return-only surface (no HTML consumer):
    // cap chunk contents here.
    cap_hits_for_return(&mut hits);

    Ok(HierarchicalResponse {
        mode: "hierarchical",
        communities: routes,
        hits,
        context_paths: flat.context_paths,
    })
}

pub async fn query_repo(
    storage: &Storage,
    repo_id: Uuid,
    embedder: &dyn Embedder,
    query: &str,
    limit: i64,
) -> Result<QueryResponse> {
    query_repo_with_expansions(storage, repo_id, embedder, query, limit, &[query], None).await
}

pub async fn query_feature_context_repo(
    storage: &Storage,
    repo_id: Uuid,
    embedder: &dyn Embedder,
    task: &str,
    limit: i64,
) -> Result<QueryResponse> {
    let expansions = feature_context_queries(task);
    let query_refs = expansions.iter().map(String::as_str).collect::<Vec<_>>();
    query_repo_with_expansions(storage, repo_id, embedder, task, limit, &query_refs, None).await
}

/// `first_query_embedding`: a precomputed embedding of `queries[0]`, when the
/// caller already paid for it (the hierarchical router) — saves one embed call.
#[allow(clippy::too_many_arguments)]
async fn query_repo_with_expansions(
    storage: &Storage,
    repo_id: Uuid,
    embedder: &dyn Embedder,
    original_query: &str,
    limit: i64,
    queries: &[&str],
    first_query_embedding: Option<&[f32]>,
) -> Result<QueryResponse> {
    let candidate_limit = (limit * 8).clamp(40, 200);
    let mut hits = Vec::new();
    for (i, query) in queries.iter().enumerate() {
        let precomputed = if i == 0 { first_query_embedding } else { None };
        let mut query_hits = retrieve_query_hits(
            storage,
            repo_id,
            embedder,
            query,
            candidate_limit,
            precomputed,
        )
        .await?;
        hits.append(&mut query_hits);
    }
    merge_duplicate_hits(&mut hits);
    rerank_hits(&mut hits, original_query);
    suppress_dependency_noise(&mut hits, original_query, limit as usize);
    retain_supplemental_context(&mut hits, limit as usize);
    hits.truncate(limit as usize);

    let node_ids = hits.iter().filter_map(|h| h.node_id).collect::<Vec<_>>();
    let edges = storage.load_edges_for_nodes(repo_id, &node_ids).await?;
    let context_paths = best_context_paths(&hits, &edges, 8);
    Ok(QueryResponse {
        hits,
        context_paths,
    })
}

async fn retrieve_query_hits(
    storage: &Storage,
    repo_id: Uuid,
    embedder: &dyn Embedder,
    query: &str,
    candidate_limit: i64,
    precomputed_embedding: Option<&[f32]>,
) -> Result<Vec<SearchHit>> {
    let query_embedding = match precomputed_embedding {
        Some(v) => v.to_vec(),
        None => embedder.embed(query).await?,
    };
    let mut hits = storage
        .semantic_search(
            repo_id,
            embedder.provider(),
            embedder.model_id(),
            embedder.dimensions(),
            &query_embedding,
            candidate_limit,
        )
        .await?;
    tag_retrieved_by(&mut hits, "semantic");
    let mut keyword_hits = storage
        .keyword_search(repo_id, query, candidate_limit)
        .await?;
    tag_retrieved_by(&mut keyword_hits, "keyword");
    merge_hits(&mut hits, keyword_hits);
    for term in literal_search_terms(query).into_iter().take(10) {
        let mut literal_hits = storage.literal_search(repo_id, &term, 12).await?;
        tag_retrieved_by(&mut literal_hits, "literal");
        merge_hits(&mut hits, literal_hits);
    }
    Ok(hits)
}

/// Stamp every hit's metadata with the retrieval method that produced it
/// (`semantic` / `keyword` / `literal`), so downstream artifacts can show *how*
/// each hit was found. Additive — appended into `metadata.retrieved_by`.
fn tag_retrieved_by(hits: &mut [SearchHit], method: &str) {
    for hit in hits.iter_mut() {
        add_retrieved_by(&mut hit.metadata, method);
    }
}

/// Append `method` to `metadata.retrieved_by` (creating the array if needed),
/// de-duplicating. Leaves non-object, non-null metadata untouched so chunk
/// metadata is never clobbered.
fn add_retrieved_by(metadata: &mut Value, method: &str) {
    if metadata.is_null() {
        *metadata = Value::Object(serde_json::Map::new());
    }
    let Some(obj) = metadata.as_object_mut() else {
        return;
    };
    let arr = obj
        .entry("retrieved_by")
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Some(list) = arr.as_array_mut() {
        let value = Value::String(method.to_string());
        if !list.contains(&value) {
            list.push(value);
        }
    }
}

/// Union an incoming `retrieved_by` array into `metadata` (used when the same
/// chunk surfaces from more than one retrieval method and the hits are merged).
fn union_retrieved_into(metadata: &mut Value, incoming: &Value) {
    if let Some(methods) = incoming.as_array() {
        for method in methods {
            if let Some(name) = method.as_str() {
                add_retrieved_by(metadata, name);
            }
        }
    }
}

fn merge_hits(base: &mut Vec<SearchHit>, keyword: Vec<SearchHit>) {
    let mut seen: HashMap<Uuid, usize> = base
        .iter()
        .enumerate()
        .map(|(idx, hit)| (hit.chunk_id, idx))
        .collect();
    for mut hit in keyword {
        if let Some(idx) = seen.get(&hit.chunk_id).copied() {
            base[idx].score += hit.score.max(0.0) * 0.25;
            if let Some(incoming) = hit.metadata.get("retrieved_by").cloned() {
                union_retrieved_into(&mut base[idx].metadata, &incoming);
            }
        } else {
            hit.score *= 0.75;
            seen.insert(hit.chunk_id, base.len());
            base.push(hit);
        }
    }
    base.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

fn merge_duplicate_hits(hits: &mut Vec<SearchHit>) {
    let mut merged: HashMap<Uuid, SearchHit> = HashMap::new();
    for hit in hits.drain(..) {
        let score = hit.score;
        let incoming = hit.metadata.get("retrieved_by").cloned();
        merged
            .entry(hit.chunk_id)
            .and_modify(|existing| {
                existing.score = existing.score.max(score) + score.max(0.0) * 0.12;
                if let Some(ref incoming) = incoming {
                    union_retrieved_into(&mut existing.metadata, incoming);
                }
            })
            .or_insert(hit);
    }
    hits.extend(merged.into_values());
}

fn rerank_hits(hits: &mut [SearchHit], query: &str) {
    let query = query.to_ascii_lowercase();
    let query_tokens = search_tokens(&query);
    let dependency_query = contains_any(
        &query,
        &[
            "dependency",
            "dependencies",
            "package",
            "library",
            "sdk",
            "version",
            "npm",
            "crate",
        ],
    );
    let deployment_query = contains_any(
        &query,
        &[
            "deploy",
            "deployment",
            "infrastructure",
            "stack",
            "cdk",
            "terraform",
            "kubernetes",
            "configured",
            "configuration",
        ],
    );

    for hit in hits.iter_mut() {
        let chunk_type = hit
            .metadata
            .get("kind")
            .or_else(|| hit.metadata.get("ecosystem"))
            .and_then(|v| v.as_str())
            .unwrap_or(&hit.content)
            .to_ascii_lowercase();
        let content = hit.content.to_ascii_lowercase();
        let file_path = hit
            .file_path
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let metadata_text = hit.metadata.to_string().to_ascii_lowercase();
        let lexical_matches = lexical_match_count(
            &query_tokens,
            &[content.as_str(), file_path.as_str(), metadata_text.as_str()],
        );
        if lexical_matches > 0 {
            hit.score *= 1.0 + (lexical_matches as f64 * 0.08).min(0.8);
        }
        if !query_tokens.is_empty()
            && query_tokens
                .iter()
                .any(|token| token.len() > 3 && file_path.contains(token))
        {
            hit.score *= 1.5;
        }

        if hit.metadata.get("dependency").is_some() && !dependency_query {
            hit.score *= 0.25;
        }
        if hit
            .metadata
            .get("source_priority")
            .and_then(|v| v.as_str())
            .is_some_and(|priority| priority == "supplemental")
        {
            hit.score *= 0.72;
        }
        if hit.metadata.get("script").is_some() {
            hit.score *= 1.25;
        }
        if hit
            .metadata
            .get("technology")
            .and_then(|v| v.as_str())
            .is_some_and(|technology| technology == "aws_cdk")
        {
            hit.score *= 1.35;
        }
        if deployment_query
            && (content.contains("deploy")
                || content.contains("stack")
                || content.contains("infrastructure")
                || content.contains("cdk")
                || content.contains("aws cdk")
                || content.contains("terraform")
                || content.contains("kubernetes"))
        {
            hit.score *= 1.6;
        }
        if chunk_type == "function" || chunk_type == "struct" || chunk_type == "trait" {
            hit.score *= 1.04;
        }
    }

    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

fn feature_context_queries(task: &str) -> Vec<String> {
    let mut queries = vec![
        task.to_string(),
        format!("{task} documentation docs README guide architecture"),
        format!("{task} source implementation contracts services hooks tests"),
        format!("{task} workflow user story infrastructure deployment configuration"),
    ];
    let normalized = task
        .replace(['-', '_'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if normalized != task {
        queries.push(normalized);
    }
    queries.dedup();
    queries
}

fn search_tokens(query: &str) -> Vec<String> {
    query
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .map(str::to_ascii_lowercase)
        .filter(|token| token.len() > 1)
        .collect::<Vec<_>>()
}

fn literal_search_terms(query: &str) -> Vec<String> {
    let mut terms = search_tokens(query)
        .into_iter()
        .filter(|token| token.len() > 2 && !LITERAL_SEARCH_STOP_TERMS.contains(&token.as_str()))
        .collect::<Vec<_>>();
    let compact = query
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    if compact.len() > 4 {
        terms.push(compact);
    }
    terms.sort();
    terms.dedup();
    terms
}

const LITERAL_SEARCH_STOP_TERMS: &[&str] = &[
    "architecture",
    "configuration",
    "contracts",
    "documentation",
    "docs",
    "feature",
    "guide",
    "implementation",
    "infrastructure",
    "readme",
    "services",
    "source",
    "story",
    "tests",
    "user",
    "workflow",
];

/// Significant query tokens for the router's label-match fallback: alphanumeric,
/// length ≥ `MIN_ROUTE_TOKEN`, minus literal-search stopwords and common question
/// words (so "how does X work" doesn't route on "work"/"the").
fn router_label_tokens(query: &str) -> Vec<String> {
    let mut tokens: Vec<String> = search_tokens(query)
        .into_iter()
        .filter(|t| {
            t.len() >= MIN_ROUTE_TOKEN
                && !LITERAL_SEARCH_STOP_TERMS.contains(&t.as_str())
                && !ROUTE_STOP_WORDS.contains(&t.as_str())
        })
        .collect();
    tokens.sort();
    tokens.dedup();
    tokens
}

/// True if any token matches a path SEGMENT of the label — exact equality (so
/// "ocl" hits the `ocl` segment of `ocl-repository.ts`) or, for tokens of length
/// ≥ 6, a segment prefix (so "onchain" hits `onchainlabs`). Segment-scoped
/// matching avoids the substring noise of a raw `label.contains(token)` (e.g.
/// "work" inside "network", "lab" inside "label"); the 6-char prefix floor
/// keeps short words from prefix-hijacking unrelated segments ("auth" must not
/// match "author").
fn label_matches_tokens(label: &str, tokens: &[String]) -> bool {
    let segments: Vec<String> = label
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect();
    tokens.iter().any(|t| {
        segments
            .iter()
            .any(|s| s == t || (t.len() >= 6 && s.starts_with(t.as_str())))
    })
}

/// Question filler plus ubiquitous path segments (`api`, `src`, …) that appear
/// in nearly every label — routing on them would always select the largest
/// communities regardless of the question.
const ROUTE_STOP_WORDS: &[&str] = &[
    "and", "api", "app", "apps", "are", "can", "does", "for", "from", "has", "hood", "how", "into",
    "its", "lib", "src", "that", "the", "this", "under", "was", "web", "what", "when", "where",
    "which", "who", "why", "with", "work", "works", "you", "your",
];

fn lexical_match_count(tokens: &[String], haystacks: &[&str]) -> usize {
    tokens
        .iter()
        .map(|token| {
            haystacks
                .iter()
                .filter(|haystack| haystack.contains(token))
                .count()
        })
        .sum()
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn suppress_dependency_noise(hits: &mut Vec<SearchHit>, query: &str, _limit: usize) {
    let query = query.to_ascii_lowercase();
    let dependency_query = contains_any(
        &query,
        &[
            "dependency",
            "dependencies",
            "package",
            "library",
            "sdk",
            "version",
            "npm",
            "crate",
        ],
    );
    if dependency_query {
        return;
    }

    let non_dependency_count = hits
        .iter()
        .filter(|hit| hit.metadata.get("dependency").is_none())
        .count();
    if non_dependency_count > 0 {
        hits.retain(|hit| hit.metadata.get("dependency").is_none());
    }
}

fn is_supplemental_doc(hit: &SearchHit) -> bool {
    hit.metadata
        .get("source_priority")
        .and_then(|v| v.as_str())
        .is_some_and(|priority| priority == "supplemental")
        || hit
            .metadata
            .get("kind")
            .and_then(|v| v.as_str())
            .is_some_and(|kind| kind == "documentation")
}

fn retain_supplemental_context(hits: &mut Vec<SearchHit>, limit: usize) {
    if limit == 0 || hits.len() <= limit {
        return;
    }
    let target_docs = (limit / 5).clamp(1, 3);
    let current_docs = hits
        .iter()
        .take(limit)
        .filter(|hit| is_supplemental_doc(hit))
        .count();
    if current_docs >= target_docs {
        return;
    }
    let mut needed = target_docs - current_docs;
    let mut insert_at = limit.saturating_sub(needed).min(hits.len());
    let mut scan_idx = limit;
    while needed > 0 && scan_idx < hits.len() {
        if is_supplemental_doc(&hits[scan_idx]) {
            let doc_hit = hits.remove(scan_idx);
            hits.insert(insert_at, doc_hit);
            insert_at += 1;
            needed -= 1;
        } else {
            scan_idx += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn truncate_for_return_caps_and_marks() {
        let short = super::truncate_for_return("hello", 10);
        assert_eq!(short, "hello");
        let long_text = "x".repeat(900);
        let capped = super::truncate_for_return(&long_text, super::MAX_RETURN_CHUNK_CHARS);
        assert!(capped.starts_with(&"x".repeat(super::MAX_RETURN_CHUNK_CHARS)));
        assert!(capped.contains("+100 chars"));
        // char-boundary safe on multibyte text
        let multi = "é".repeat(900);
        let capped = super::truncate_for_return(&multi, 800);
        assert!(capped.chars().count() < 900 + 40);
    }

    use super::*;
    use serde_json::json;

    #[test]
    fn suppresses_dependencies_for_non_dependency_queries() {
        let mut hits = vec![
            hit(json!({"dependency": "@aws-sdk/client-lambda"}), 0.9),
            hit(json!({"script": "deploy"}), 0.8),
            hit(json!({"kind": "function"}), 0.7),
        ];

        rerank_hits(&mut hits, "where is infrastructure deployment configured?");
        suppress_dependency_noise(
            &mut hits,
            "where is infrastructure deployment configured?",
            10,
        );

        assert_eq!(hits.len(), 2);
        assert!(hits
            .iter()
            .all(|hit| hit.metadata.get("dependency").is_none()));
    }

    #[test]
    fn keeps_dependencies_for_dependency_queries() {
        let mut hits = vec![
            hit(json!({"dependency": "@aws-sdk/client-lambda"}), 0.9),
            hit(json!({"script": "deploy"}), 0.8),
        ];

        suppress_dependency_noise(&mut hits, "which aws sdk dependencies are used?", 1);

        assert!(hits
            .iter()
            .any(|hit| hit.metadata.get("dependency").is_some()));
    }

    #[test]
    fn router_tokens_drop_question_noise_keep_signal() {
        let t = router_label_tokens("How does OCL (Onchain Lab) work under the hood?");
        assert!(t.contains(&"ocl".to_string()));
        assert!(t.contains(&"onchain".to_string()));
        assert!(t.contains(&"lab".to_string()));
        // Common question/filler words must be dropped so they can't route labels.
        for noise in ["how", "does", "work", "under", "the", "hood"] {
            assert!(
                !t.contains(&noise.to_string()),
                "{noise} should be filtered"
            );
        }
    }

    #[test]
    fn label_match_is_segment_scoped() {
        let tokens = vec!["ocl".to_string(), "onchain".to_string()];
        // Exact segment match ("ocl" in ocl.ts) and prefix for longer tokens.
        assert!(label_matches_tokens("desci/common/domains/ocl.ts", &tokens));
        assert!(label_matches_tokens("onchainlabs/src/Foo.sol", &tokens));
        // No substring false positives: "work" ⊄ "network", "lab" ⊄ "label".
        assert!(!label_matches_tokens(
            "ui/network/label-control.tsx",
            &["work".to_string(), "lab".to_string()]
        ));
        // A short token only matches a whole segment, never a prefix.
        assert!(!label_matches_tokens(
            "onchainlabs/x.ts",
            &["lab".to_string()]
        ));
    }

    #[test]
    fn keeps_one_matching_supplemental_doc_in_limited_context() {
        let mut hits = vec![
            hit(json!({"kind": "function"}), 0.95),
            hit(json!({"kind": "struct"}), 0.9),
            hit(json!({"kind": "module"}), 0.85),
            hit(
                json!({"kind": "documentation", "source_priority": "supplemental"}),
                0.4,
            ),
        ];

        retain_supplemental_context(&mut hits, 3);
        hits.truncate(3);

        assert!(hits.iter().any(is_supplemental_doc));
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn feature_context_queries_request_docs_and_infra() {
        let queries = feature_context_queries("OCL On-Chain Lab");

        assert!(queries.iter().any(|query| query.contains("documentation")));
        assert!(queries.iter().any(|query| query.contains("infrastructure")));
        assert!(queries.iter().any(|query| query.contains("contracts")));
    }

    #[test]
    fn path_matches_boost_hits() {
        let mut hits = vec![
            hit(json!({"kind": "function"}), 0.7),
            SearchHit {
                file_path: Some("onchainlabs/src/OnChainLab.sol".into()),
                content: "contract body".into(),
                ..hit(json!({"kind": "documentation"}), 0.5)
            },
        ];

        rerank_hits(&mut hits, "OnChainLab architecture");

        assert_eq!(
            hits[0].file_path.as_deref(),
            Some("onchainlabs/src/OnChainLab.sol")
        );
    }

    #[test]
    fn literal_terms_include_compact_path_form() {
        let terms = literal_search_terms("On-Chain Lab");

        assert!(terms.contains(&"onchainlab".to_string()));
    }

    #[test]
    fn literal_terms_drop_generic_context_words() {
        let terms = literal_search_terms("OCL documentation docs architecture");

        assert!(terms.contains(&"ocl".to_string()));
        assert!(!terms.contains(&"docs".to_string()));
        assert!(!terms.contains(&"documentation".to_string()));
        assert!(!terms.contains(&"architecture".to_string()));
    }

    #[test]
    fn tags_retrieval_methods_and_dedups() {
        let mut hits = vec![hit(json!({"kind": "function"}), 0.5)];
        tag_retrieved_by(&mut hits, "semantic");
        add_retrieved_by(&mut hits[0].metadata, "keyword");
        add_retrieved_by(&mut hits[0].metadata, "semantic"); // duplicate ignored

        let methods = hits[0]
            .metadata
            .get("retrieved_by")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(methods.len(), 2);
        assert!(methods.iter().any(|m| m.as_str() == Some("semantic")));
        assert!(methods.iter().any(|m| m.as_str() == Some("keyword")));
        // Existing chunk metadata is preserved.
        assert_eq!(
            hits[0].metadata.get("kind").and_then(|v| v.as_str()),
            Some("function")
        );
    }

    fn hit(metadata: serde_json::Value, score: f64) -> SearchHit {
        SearchHit {
            chunk_id: Uuid::new_v4(),
            node_id: Some(Uuid::new_v4()),
            file_path: Some("package.json".into()),
            line_start: Some(1),
            line_end: Some(1),
            score,
            content: "deploy stack cdk".into(),
            metadata,
        }
    }
}
