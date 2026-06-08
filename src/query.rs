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

/// Minimum cosine score for a community to count as a matched feature.
const COMMUNITY_MATCH_FLOOR: f64 = 0.30;

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
    let routes: Vec<CommunityRoute> = matches
        .iter()
        .filter(|m| m.score >= COMMUNITY_MATCH_FLOOR && m.member_count >= 2)
        .map(|m| CommunityRoute {
            id: m.id,
            label: m.label.clone(),
            member_count: m.member_count,
            score: m.score,
            summary: m.summary.clone(),
        })
        .collect();

    let flat = query_repo(storage, repo_id, embedder, query, limit).await?;
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
    query_repo_with_expansions(storage, repo_id, embedder, query, limit, &[query]).await
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
    query_repo_with_expansions(storage, repo_id, embedder, task, limit, &query_refs).await
}

async fn query_repo_with_expansions(
    storage: &Storage,
    repo_id: Uuid,
    embedder: &dyn Embedder,
    original_query: &str,
    limit: i64,
    queries: &[&str],
) -> Result<QueryResponse> {
    let candidate_limit = (limit * 8).clamp(40, 200);
    let mut hits = Vec::new();
    for query in queries {
        let mut query_hits =
            retrieve_query_hits(storage, repo_id, embedder, query, candidate_limit).await?;
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
) -> Result<Vec<SearchHit>> {
    let query_embedding = embedder.embed(query).await?;
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
