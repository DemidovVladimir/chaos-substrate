mod config;
mod embedding;
mod extractor;
mod feature_context;
mod feature_export;
mod graph;
mod graph_export;
mod mcp;
mod models;
mod obsidian_export;
mod query;
mod simple_graph_optimizer;
mod storage;

pub use config::Config;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use embedding::build_embedder;
use extractor::{current_commit, RustRepositoryExtractor};
use feature_context::{
    build_feature_context_warnings, load_feature_matches, write_feature_context_html,
    FeatureContextResponse,
};
use feature_export::refresh_project_exports;
use graph_export::write_graph_html;
use obsidian_export::write_obsidian_vault;
use query::{query_feature_context_repo, query_repo};
use serde_json::json;
use std::path::PathBuf;
use storage::Storage;

#[derive(Parser)]
#[command(name = "chaos")]
#[command(about = "Persistent code knowledge memory for agents")]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Apply database migrations.
    Migrate,
    /// Verify database and embedder configuration.
    Doctor,
    /// Analyze and persist a repository knowledge graph and embeddings.
    Analyze { repo_path: PathBuf },
    /// Query an already indexed repository.
    Query {
        repo: String,
        question: String,
        #[arg(long, default_value_t = 10)]
        limit: i64,
    },
    /// Build an implementation context from Postgres retrieval and generated feature pages.
    FeatureContext {
        repo: String,
        task: String,
        #[arg(long)]
        features_dir: Option<PathBuf>,
        #[arg(long)]
        output_html: Option<PathBuf>,
        #[arg(long, default_value_t = 10)]
        limit: i64,
        #[arg(long, default_value_t = 3)]
        feature_limit: usize,
        #[arg(long, default_value_t = 8)]
        nodes_per_feature: usize,
    },
    /// Export an indexed repository as an interactive standalone HTML graph.
    Graph {
        repo: String,
        #[arg(short, long, default_value = "graph.html")]
        output: PathBuf,
    },
    /// Export an indexed repository as an Obsidian vault.
    Obsidian {
        repo: String,
        #[arg(short, long, default_value = "chaos-obsidian-vault")]
        output: PathBuf,
    },
    /// Refresh generated Obsidian artifacts from the persisted index.
    Refresh {
        repo: String,
        #[arg(long)]
        obsidian_output: Option<PathBuf>,
        #[arg(long)]
        features_dir: Option<PathBuf>,
        #[arg(long)]
        all_features: bool,
    },
    /// Run the stdio MCP server.
    Mcp,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load(cli.config.as_deref())?;

    match cli.command {
        Commands::Migrate => {
            let storage = Storage::connect(&config.storage.database_url).await?;
            storage.migrate().await?;
            println!("migrations applied");
        }
        Commands::Doctor => {
            let storage = Storage::connect(&config.storage.database_url).await?;
            let health = storage.health().await?;
            let embedder = build_embedder(&config.embedding)?;
            let probe = embedder.embed("chaos substrate doctor probe").await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "storage": health,
                    "embedder": {
                        "provider": embedder.provider(),
                        "model": embedder.model_id(),
                        "dimensions": embedder.dimensions(),
                        "probe_dimensions": probe.len()
                    }
                }))?
            );
        }
        Commands::Analyze { repo_path } => {
            let storage = Storage::connect(&config.storage.database_url).await?;
            let embedder = build_embedder(&config.embedding)?;
            let commit = current_commit(&repo_path);
            let repo = storage
                .upsert_repository(&repo_path, commit.as_deref())
                .await?;
            let run_id = storage.begin_analysis(repo.id, commit.as_deref()).await?;
            let outcome = async {
                let extractor = RustRepositoryExtractor::new(config.indexing.clone());
                let result = extractor.extract(&repo_path, repo.id, commit)?;
                storage.replace_repo_index(repo.id, &result).await?;
                let missing = storage
                    .chunks_missing_embeddings(
                        repo.id,
                        embedder.provider(),
                        embedder.model_id(),
                        embedder.dimensions(),
                    )
                    .await?;
                for chunk in &missing {
                    let embedding = embedder.embed(&chunk.content).await?;
                    storage
                        .insert_embedding(
                            chunk,
                            embedder.provider(),
                            embedder.model_id(),
                            embedder.dimensions(),
                            &embedding,
                        )
                        .await?;
                }
                Result::<_, anyhow::Error>::Ok((result, missing.len()))
            }
            .await;

            match outcome {
                Ok((result, embedded)) => {
                    storage.finish_analysis(run_id, "completed", None).await?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "repo_id": repo.id,
                            "files": result.files.len(),
                            "nodes": result.nodes.len(),
                            "edges": result.edges.len(),
                            "chunks": result.chunks.len(),
                            "embedded_chunks": embedded
                        }))?
                    );
                }
                Err(err) => {
                    storage
                        .finish_analysis(run_id, "failed", Some(&err.to_string()))
                        .await?;
                    return Err(err);
                }
            }
        }
        Commands::Query {
            repo,
            question,
            limit,
        } => {
            let storage = Storage::connect(&config.storage.database_url).await?;
            let embedder = build_embedder(&config.embedding)?;
            let repo = storage
                .find_repository(&repo)
                .await?
                .with_context(|| format!("repository is not indexed: {repo}"))?;
            let response =
                query_repo(&storage, repo.id, embedder.as_ref(), &question, limit).await?;
            println!("{}", serde_json::to_string_pretty(&response)?);
        }
        Commands::FeatureContext {
            repo,
            task,
            features_dir,
            output_html,
            limit,
            feature_limit,
            nodes_per_feature,
        } => {
            let storage = Storage::connect(&config.storage.database_url).await?;
            let embedder = build_embedder(&config.embedding)?;
            let repo = storage
                .find_repository(&repo)
                .await?
                .with_context(|| format!("repository is not indexed: {repo}"))?;
            let repo_root = PathBuf::from(&repo.root_path);
            let features_dir =
                features_dir.unwrap_or_else(|| repo_root.join("docs/features_memory"));
            let postgres =
                query_feature_context_repo(&storage, repo.id, embedder.as_ref(), &task, limit)
                    .await?;
            let warnings = build_feature_context_warnings(&task, &repo_root, &postgres);
            let feature_matches =
                load_feature_matches(&task, &features_dir, feature_limit, nodes_per_feature)?;
            let response = FeatureContextResponse {
                task,
                postgres,
                features_dir,
                warnings,
                feature_matches,
            };
            if let Some(output_html) = output_html {
                write_feature_context_html(&output_html, &response)?;
            }
            println!("{}", serde_json::to_string_pretty(&response)?);
        }
        Commands::Graph { repo, output } => {
            let storage = Storage::connect(&config.storage.database_url).await?;
            let repo = storage
                .find_repository(&repo)
                .await?
                .with_context(|| format!("repository is not indexed: {repo}"))?;
            let graph = storage.load_graph_export(&repo).await?;
            write_graph_html(&output, &graph)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "output": output,
                    "repo_id": repo.id,
                    "nodes": graph.nodes.len(),
                    "edges": graph.edges.len()
                }))?
            );
        }
        Commands::Obsidian { repo, output } => {
            let storage = Storage::connect(&config.storage.database_url).await?;
            let repo = storage
                .find_repository(&repo)
                .await?
                .with_context(|| format!("repository is not indexed: {repo}"))?;
            let graph = storage.load_graph_export(&repo).await?;
            let summary = write_obsidian_vault(&output, &graph)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "output": summary.output,
                    "repo_id": repo.id,
                    "topics": summary.topics,
                    "node_notes": summary.node_notes,
                    "edges": summary.edges
                }))?
            );
        }
        Commands::Refresh {
            repo,
            obsidian_output,
            features_dir,
            all_features,
        } => {
            let storage = Storage::connect(&config.storage.database_url).await?;
            let repo = storage
                .find_repository(&repo)
                .await?
                .with_context(|| format!("repository is not indexed: {repo}"))?;
            let graph = storage.load_graph_export(&repo).await?;
            let repo_root = PathBuf::from(&repo.root_path);
            let obsidian_output =
                obsidian_output.unwrap_or_else(|| repo_root.join("chaos-obsidian-vault"));
            let features_dir =
                features_dir.unwrap_or_else(|| repo_root.join("docs/features_memory"));
            let summary =
                refresh_project_exports(&graph, &obsidian_output, &features_dir, all_features)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "repo_id": repo.id,
                    "obsidian": {
                        "output": summary.obsidian.output,
                        "topics": summary.obsidian.topics,
                        "node_notes": summary.obsidian.node_notes,
                        "edges": summary.obsidian.edges
                    },
                    "features_dir": features_dir,
                    "feature_pages": summary.feature_pages,
                    "skipped_feature_pages": summary.skipped_feature_pages
                }))?
            );
        }
        Commands::Mcp => {
            mcp::run(config).await?;
        }
    }

    Ok(())
}
