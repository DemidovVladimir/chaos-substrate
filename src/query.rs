use crate::{
    embedding::Embedder,
    graph::{best_context_paths, ContextPath},
    models::SearchHit,
    storage::Storage,
};
use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct QueryResponse {
    pub hits: Vec<SearchHit>,
    pub context_paths: Vec<ContextPath>,
}

pub async fn query_repo(
    storage: &Storage,
    repo_id: Uuid,
    embedder: &dyn Embedder,
    query: &str,
    limit: i64,
) -> Result<QueryResponse> {
    let query_embedding = embedder.embed(query).await?;
    let candidate_limit = (limit * 8).clamp(40, 200);
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
    let keyword_hits = storage
        .keyword_search(repo_id, query, candidate_limit)
        .await?;
    merge_hits(&mut hits, keyword_hits);
    rerank_hits(&mut hits, query);
    suppress_dependency_noise(&mut hits, query, limit as usize);
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

fn merge_hits(base: &mut Vec<SearchHit>, keyword: Vec<SearchHit>) {
    let mut seen: HashMap<Uuid, usize> = base
        .iter()
        .enumerate()
        .map(|(idx, hit)| (hit.chunk_id, idx))
        .collect();
    for mut hit in keyword {
        if let Some(idx) = seen.get(&hit.chunk_id) {
            base[*idx].score += hit.score.max(0.0) * 0.25;
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

fn rerank_hits(hits: &mut [SearchHit], query: &str) {
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
    if limit == 0 || hits.len() <= limit || hits.iter().take(limit).any(is_supplemental_doc) {
        return;
    }
    let Some(doc_idx) = hits.iter().position(is_supplemental_doc) else {
        return;
    };
    let doc_hit = hits.remove(doc_idx);
    let insert_at = limit.saturating_sub(1).min(hits.len());
    hits.insert(insert_at, doc_hit);
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
