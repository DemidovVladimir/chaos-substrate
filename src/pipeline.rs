//! Shared CLI/MCP pipelines.
//!
//! `chaos <command>` and the matching `chaos_*` MCP tool call the SAME
//! function here, so the two surfaces cannot drift (the `run_clean`
//! precedent, generalized). Each function returns the summary `Value`; the
//! caller decides how to print it (stdout JSON vs MCP tool text).

use crate::{
    community, community_summary,
    embedding::{self, Embedder},
    export_util::features_memory_dir,
    extractor::{current_commit, RustRepositoryExtractor},
    feature_export::refresh_project_exports,
    hierarchy_export, merkle,
    obsidian_export::write_obsidian_vault,
    project, query,
    storage::Storage,
    Config,
};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The full analyze pipeline: extract → persist graph/chunks → embed missing →
/// L1 communities → L2 Merkle roots → L3 hash-gated summaries. The analysis
/// run is marked completed FIRST, then the repo's projects are relinked
/// (best-effort) — a slow P6 pass can never leave the run unfinished.
pub(crate) async fn run_analyze(
    config: &Config,
    storage: &Storage,
    embedder: &dyn Embedder,
    repo_path: &Path,
) -> Result<Value> {
    let commit = current_commit(repo_path);
    let repo = storage
        .upsert_repository(repo_path, commit.as_deref())
        .await?;
    let run_id = storage.begin_analysis(repo.id, commit.as_deref()).await?;
    let outcome = async {
        let extractor = RustRepositoryExtractor::new(config.indexing.clone());
        let result = extractor.extract(repo_path, repo.id, commit)?;
        // Embeddings for unchanged content survive the wipe (restored by
        // content hash inside the replace transaction) — only genuinely
        // new/changed chunks are left to embed.
        let reused = storage.replace_repo_index(repo.id, &result).await?;
        let missing = storage
            .chunks_missing_embeddings(
                repo.id,
                embedder.provider(),
                embedder.model_id(),
                embedder.dimensions(),
            )
            .await?;
        embedding::embed_missing_chunks(storage, embedder, &missing).await?;
        // L1: derive + persist the community layer from the written graph.
        let detection =
            community::detect_and_persist(storage, repo.id, &community::CommunityConfig::default())
                .await?;
        // L2: roll the content-hash leaves up to file/community/repo roots.
        let merkle = merkle::compute_and_persist(storage, repo.id).await?;
        // L3: hash-gated community summaries, embedded by the real embedder.
        let summary = community_summary::summarize_repo(storage, embedder, repo.id).await?;
        Result::<_, anyhow::Error>::Ok((result, reused, missing.len(), detection, merkle, summary))
    }
    .await;

    match outcome {
        Ok((result, reused_embeddings, embedded, detection, merkle, summary)) => {
            storage.finish_analysis(run_id, "completed", None).await?;
            // P6: keep the project layer fresh — relink every project
            // containing this repo (hash-gated; empty when none).
            let projects = project::relink_projects_for_repo(storage, repo.id).await;
            let feature_communities = detection.communities.iter().filter(|c| c.size >= 2).count();
            Ok(json!({
                "repo_id": repo.id,
                "files": result.files.len(),
                "nodes": result.nodes.len(),
                "edges": result.edges.len(),
                "chunks": result.chunks.len(),
                "embedded_chunks": embedded,
                "reused_embeddings": reused_embeddings,
                "communities": detection.communities.len(),
                "feature_communities": feature_communities,
                "quotient_edges": detection.quotient_edges.len(),
                "modularity": detection.modularity,
                "repo_root_hash": merkle.repo_root_hash,
                "summaries": {
                    "summarized": summary.summarized,
                    "skipped": summary.skipped,
                    "embed_calls": summary.embed_calls,
                    "reused_from_cache": summary.reused
                },
                "projects": projects
            }))
        }
        Err(err) => {
            storage
                .finish_analysis(run_id, "failed", Some(&err.to_string()))
                .await?;
            Err(err)
        }
    }
}

/// Query glue: resolve the repo, route hierarchical vs flat, and cap the flat
/// hits' chunk contents for the return surface (the full text stays in the
/// index; the agent can open the file).
pub(crate) async fn run_query(
    storage: &Storage,
    embedder: &dyn Embedder,
    repo: &str,
    question: &str,
    limit: i64,
    hierarchical: bool,
) -> Result<Value> {
    let repository = storage
        .find_repository(repo)
        .await?
        .with_context(|| format!("repository is not indexed: {repo}"))?;
    if hierarchical {
        let response =
            query::query_repo_hierarchical(storage, repository.id, embedder, question, limit)
                .await?;
        Ok(serde_json::to_value(response)?)
    } else {
        let mut response =
            query::query_repo(storage, repository.id, embedder, question, limit).await?;
        query::cap_hits_for_return(&mut response.hits);
        Ok(serde_json::to_value(response)?)
    }
}

/// Obsidian vault + hierarchy export from the persisted graph. `output` = None
/// defaults to `<repo>/chaos-obsidian-vault` (the CLI passes its own clap
/// default through, preserving its cwd-relative behavior).
pub(crate) async fn run_obsidian(
    storage: &Storage,
    repo: &str,
    output: Option<PathBuf>,
) -> Result<Value> {
    let repository = storage
        .find_repository(repo)
        .await?
        .with_context(|| format!("repository is not indexed: {repo}"))?;
    let output =
        output.unwrap_or_else(|| PathBuf::from(&repository.root_path).join("chaos-obsidian-vault"));
    let graph = storage.load_graph_export(&repository).await?;
    let summary = write_obsidian_vault(&output, &graph)?;
    let hierarchy = storage.load_community_hierarchy(&repository, 14).await?;
    let hier = hierarchy_export::write_hierarchy(&output, &output, &hierarchy)?;
    Ok(json!({
        "output": summary.output,
        "repo_id": repository.id,
        "topics": summary.topics,
        "node_notes": summary.node_notes,
        "edges": summary.edges,
        "community_notes": hier.community_notes,
        "feature_map_html": hier.feature_map_html
    }))
}

/// Regenerate project-local artifacts (vault, feature pages, feature map)
/// from the persisted index without re-indexing or embedding.
pub(crate) async fn run_refresh(
    storage: &Storage,
    repo: &str,
    obsidian_output: Option<PathBuf>,
    features_dir: Option<PathBuf>,
    all_features: bool,
) -> Result<Value> {
    let repository = storage
        .find_repository(repo)
        .await?
        .with_context(|| format!("repository is not indexed: {repo}"))?;
    let repo_root = PathBuf::from(&repository.root_path);
    let obsidian_output = obsidian_output.unwrap_or_else(|| repo_root.join("chaos-obsidian-vault"));
    let features_dir = features_dir.unwrap_or_else(|| features_memory_dir(&repo_root));
    let graph = storage.load_graph_export(&repository).await?;
    let hierarchy = storage.load_community_hierarchy(&repository, 14).await?;
    let summary = refresh_project_exports(
        &graph,
        &obsidian_output,
        &features_dir,
        all_features,
        &repo_root,
        Some(&hierarchy),
    )?;
    Ok(json!({
        "repo_id": repository.id,
        "obsidian": {
            "output": summary.obsidian.output,
            "topics": summary.obsidian.topics,
            "node_notes": summary.obsidian.node_notes,
            "edges": summary.obsidian.edges
        },
        "features_dir": features_dir,
        "feature_pages": summary.feature_pages,
        "skipped_feature_pages": summary.skipped_feature_pages,
        "community_notes": summary.community_notes,
        "feature_map_html": summary.feature_map_html
    }))
}
