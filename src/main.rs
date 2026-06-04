mod add;
mod change_plan;
mod community;
mod community_summary;
mod config;
mod embedding;
mod export_util;
mod extractor;
mod feature_context;
mod feature_export;
mod graph;
mod graph_export;
mod hook;
mod impact;
mod lang;
mod mcp;
mod merkle;
mod models;
mod obsidian_export;
mod query;
mod setup;
mod simple_graph_optimizer;
mod storage;
mod user_story;
mod weights;

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
use futures::{StreamExt, TryStreamExt};
use graph_export::write_graph_html;
use obsidian_export::write_obsidian_vault;
use query::{query_feature_context_repo, query_repo};
use serde_json::json;
use std::path::PathBuf;
use storage::Storage;
use tracing::warn;
use tracing_subscriber::EnvFilter;

/// Maximum number of embedding requests in flight at once.
pub(crate) const EMBED_CONCURRENCY: usize = 8;

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
    /// Wipe the persisted index. With no argument clears every repository;
    /// pass a repo path/name to clear only that repository.
    Clean {
        /// Optional repository path or name to clear; omit to clear everything.
        repo: Option<String>,
    },
    /// Analyze and persist a repository knowledge graph and embeddings.
    Analyze { repo_path: PathBuf },
    /// Index files changed in git (or explicit paths), refresh the Obsidian
    /// vault, and write an interactive feature/bug page — all in one shot.
    /// No file list needed; the git working tree (or `--since <ref>`) drives it.
    Add {
        /// Repository to operate on.
        #[arg(default_value = ".")]
        repo_path: PathBuf,
        /// Explicit file(s) to index; overrides git-diff detection. Repeatable.
        #[arg(long = "path")]
        paths: Vec<PathBuf>,
        /// Diff against this git ref instead of the working tree (e.g. HEAD~1, main).
        #[arg(long)]
        since: Option<String>,
        /// Force classification: `feature` or `bug`. Auto-detected from git if omitted.
        #[arg(long)]
        kind: Option<String>,
        /// Short title/summary of the change (drives the page title and slug).
        #[arg(short = 'm', long)]
        message: Option<String>,
        /// Obsidian vault output directory (default <repo>/chaos-obsidian-vault).
        #[arg(long)]
        obsidian_output: Option<PathBuf>,
        /// Skip refreshing the Obsidian vault.
        #[arg(long)]
        no_obsidian: bool,
        /// Skip writing the feature/bug page.
        #[arg(long)]
        no_page: bool,
    },
    /// Report index statistics for an indexed repository: totals plus
    /// breakdowns of nodes by kind, edges by kind, chunks by type, and files by
    /// language. Read-only; explains what an analyze/add produced.
    Stats { repo: String },
    /// Query an already indexed repository.
    Query {
        repo: String,
        question: String,
        #[arg(long, default_value_t = 10)]
        limit: i64,
        /// Top-down retrieval: route through matched features (L1 communities)
        /// first, then drill into chunks. Falls back to flat when absent.
        #[arg(long)]
        hierarchical: bool,
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
    /// Build a feature-vs-existing-code impact report and ALWAYS write an
    /// interactive HTML (impact summary + evidence) into docs/features_memory.
    /// Shows how a feature maps onto the codebase as it is today (the "before").
    Impact {
        repo: String,
        feature: String,
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
    /// Decompose a change into the features (communities) it spans, with a
    /// dependency-aware check order. Matches the description against community
    /// summaries (and optionally a git diff), then writes an interactive plan
    /// HTML into docs/features_memory and prints a compact JSON summary.
    ChangePlan {
        repo: String,
        /// Plain-language description of the change.
        change: String,
        /// Also seed from files changed vs this git ref (e.g. HEAD, main).
        #[arg(long)]
        since: Option<String>,
        /// Override the default docs/features_memory/<slug>-plan.html path.
        #[arg(long)]
        output_html: Option<PathBuf>,
        /// Max features to surface.
        #[arg(long, default_value_t = 8)]
        limit: usize,
    },
    /// Render a client/user-facing interactive storyboard (UI/UX user stories,
    /// no code) from a storyboard manifest JSON file into
    /// docs/features_memory/<slug>-story.html. Agents normally compose the
    /// manifest via the MCP `chaos_write_storyboard` tool; this CLI path renders
    /// a manifest you already have on disk.
    Storyboard {
        repo: String,
        /// Path to a storyboard manifest JSON file.
        #[arg(long)]
        manifest: PathBuf,
        /// Write to this exact path instead of docs/features_memory/<slug>-story.html.
        #[arg(long)]
        output_html: Option<PathBuf>,
        /// Slug for the default output filename. Defaults to the manifest feature id or title.
        #[arg(long)]
        slug: Option<String>,
        /// Page title override. Defaults to the manifest title.
        #[arg(long)]
        title: Option<String>,
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
    /// Debug-only: detect L1 communities (god-nodes) over an indexed repo and
    /// print them as JSON. Read-only; writes nothing. The P0 spike for the
    /// hierarchical-memory layer.
    #[command(hide = true)]
    Communities {
        repo: String,
        /// Modularity resolution γ (default 1.0; higher = finer communities).
        #[arg(long, default_value_t = 1.0)]
        resolution: f64,
        /// Cap the number of communities printed (largest first). 0 = all.
        #[arg(long, default_value_t = 0)]
        top: usize,
    },
    /// Run the stdio MCP server.
    Mcp,
    /// Auto-detect AI coding editors and register chaos-substrate as an MCP server in each.
    Setup {
        /// Print what would be written/run without making any changes.
        #[arg(long)]
        dry_run: bool,
        /// Scope passed to `claude mcp add` (user | local | project). Defaults to "user".
        #[arg(long)]
        scope: Option<String>,
    },
    /// Claude Code / Cursor plugin hook: reads an event JSON from stdin and
    /// injects code-memory context into the response (or exits 0 silently).
    Hook {
        /// The hook event to handle: PreToolUse or PostToolUse.
        #[arg(long)]
        event: String,
        /// Output format: "claude" (default) or "cursor".
        #[arg(long)]
        format: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Diagnostics go to stderr only. The `mcp` subcommand speaks
    // newline-delimited JSON-RPC on stdout, so any log on stdout would corrupt
    // the protocol — keep the writer pinned to stderr.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    // For the Hook subcommand the robustness contract demands exit 0 on *any*
    // problem — including a missing or malformed config file.  Detect this early:
    // if config loading fails and the subcommand is Hook, skip the config
    // (hook.rs reads DATABASE_URL directly from env) and proceed to the hook
    // branch rather than propagating an Err that would exit non-zero.
    if let Commands::Hook {
        ref event,
        ref format,
    } = cli.command
    {
        match Config::load(cli.config.as_deref()) {
            Ok(cfg) => {
                if std::env::var("DATABASE_URL").is_err() {
                    std::env::set_var("DATABASE_URL", &cfg.storage.database_url);
                }
            }
            Err(e) => {
                // Config unavailable — hook.rs will use DATABASE_URL from env or
                // fall back to the hardcoded default URL.  Log to stderr only.
                warn!("chaos hook: config load failed ({e:#}), proceeding with env defaults");
            }
        }
        hook::run(event, format.as_deref()).await;
        // Always exit 0 — the hook must never break the host tool call.
        std::process::exit(0);
    }

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
        Commands::Clean { repo } => {
            let storage = Storage::connect(&config.storage.database_url).await?;
            let summary = if let Some(repo) = repo {
                let repository = storage
                    .find_repository(&repo)
                    .await?
                    .with_context(|| format!("repository is not indexed: {repo}"))?;
                storage.purge_repository(repository.id).await?;
                json!({
                    "cleared": "repository",
                    "root_path": repository.root_path,
                    "repo_id": repository.id,
                })
            } else {
                let removed = storage.clear_all().await?;
                json!({ "cleared": "all", "removed": removed })
            };
            println!("{}", serde_json::to_string_pretty(&summary)?);
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
                let embedder_ref = embedder.as_ref();
                let storage_ref = &storage;
                futures::stream::iter(missing.iter().map(|chunk| async move {
                    let embedding = embedder_ref.embed(&chunk.content).await?;
                    storage_ref
                        .insert_embedding(
                            chunk,
                            embedder_ref.provider(),
                            embedder_ref.model_id(),
                            embedder_ref.dimensions(),
                            &embedding,
                        )
                        .await?;
                    Result::<_, anyhow::Error>::Ok(())
                }))
                .buffer_unordered(EMBED_CONCURRENCY)
                .try_collect::<()>()
                .await?;
                // L1: derive + persist the community layer from the written graph.
                let detection = community::detect_and_persist(
                    &storage,
                    repo.id,
                    &community::CommunityConfig::default(),
                )
                .await?;
                // L2: roll the content-hash leaves up to file/community/repo roots.
                let merkle = merkle::compute_and_persist(&storage, repo.id).await?;
                // L3: hash-gated community summaries, embedded by the real embedder.
                let summary =
                    community_summary::summarize_repo(&storage, embedder.as_ref(), repo.id).await?;
                Result::<_, anyhow::Error>::Ok((result, missing.len(), detection, merkle, summary))
            }
            .await;

            match outcome {
                Ok((result, embedded, detection, merkle, summary)) => {
                    storage.finish_analysis(run_id, "completed", None).await?;
                    let feature_communities =
                        detection.communities.iter().filter(|c| c.size >= 2).count();
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "repo_id": repo.id,
                            "files": result.files.len(),
                            "nodes": result.nodes.len(),
                            "edges": result.edges.len(),
                            "chunks": result.chunks.len(),
                            "embedded_chunks": embedded,
                            "communities": detection.communities.len(),
                            "feature_communities": feature_communities,
                            "quotient_edges": detection.quotient_edges.len(),
                            "modularity": detection.modularity,
                            "repo_root_hash": merkle.repo_root_hash,
                            "summaries": {
                                "summarized": summary.summarized,
                                "skipped": summary.skipped,
                                "embed_calls": summary.embed_calls
                            }
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
        Commands::Add {
            repo_path,
            paths,
            since,
            kind,
            message,
            obsidian_output,
            no_obsidian,
            no_page,
        } => {
            let storage = Storage::connect(&config.storage.database_url).await?;
            let embedder = build_embedder(&config.embedding)?;
            let opts = add::AddOptions {
                paths,
                since,
                kind,
                message,
                obsidian_output,
                no_obsidian,
                no_page,
            };
            let summary = add::run(&config, &storage, embedder.as_ref(), &repo_path, &opts).await?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        Commands::Stats { repo } => {
            let storage = Storage::connect(&config.storage.database_url).await?;
            let repo = storage
                .find_repository(&repo)
                .await?
                .with_context(|| format!("repository is not indexed: {repo}"))?;
            let stats = storage.repo_stats(&repo).await?;
            println!("{}", serde_json::to_string_pretty(&stats)?);
        }
        Commands::Query {
            repo,
            question,
            limit,
            hierarchical,
        } => {
            let storage = Storage::connect(&config.storage.database_url).await?;
            let embedder = build_embedder(&config.embedding)?;
            let repo = storage
                .find_repository(&repo)
                .await?
                .with_context(|| format!("repository is not indexed: {repo}"))?;
            if hierarchical {
                let response = query::query_repo_hierarchical(
                    &storage,
                    repo.id,
                    embedder.as_ref(),
                    &question,
                    limit,
                )
                .await?;
                println!("{}", serde_json::to_string_pretty(&response)?);
            } else {
                let response =
                    query_repo(&storage, repo.id, embedder.as_ref(), &question, limit).await?;
                println!("{}", serde_json::to_string_pretty(&response)?);
            }
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
        Commands::Impact {
            repo,
            feature,
            features_dir,
            output_html,
            limit,
            feature_limit,
            nodes_per_feature,
        } => {
            let storage = Storage::connect(&config.storage.database_url).await?;
            let embedder = build_embedder(&config.embedding)?;
            let opts = impact::ImpactOptions {
                features_dir,
                output_html,
                limit,
                feature_limit,
                nodes_per_feature,
            };
            let summary = impact::run(&storage, embedder.as_ref(), &repo, &feature, &opts).await?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        Commands::ChangePlan {
            repo,
            change,
            since,
            output_html,
            limit,
        } => {
            let storage = Storage::connect(&config.storage.database_url).await?;
            let embedder = build_embedder(&config.embedding)?;
            let opts = change_plan::ChangePlanOptions {
                output_html,
                diff_since: since,
                limit,
            };
            let summary =
                change_plan::run(&storage, embedder.as_ref(), &repo, &change, &opts).await?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        Commands::Storyboard {
            repo,
            manifest,
            output_html,
            slug,
            title,
        } => {
            let storage = Storage::connect(&config.storage.database_url).await?;
            let repo = storage
                .find_repository(&repo)
                .await?
                .with_context(|| format!("repository is not indexed: {repo}"))?;
            let raw = std::fs::read_to_string(&manifest)
                .with_context(|| format!("reading storyboard manifest {}", manifest.display()))?;
            let manifest: user_story::StoryboardManifest =
                serde_json::from_str(&raw).context("parsing storyboard manifest JSON")?;
            let title = title.unwrap_or_else(|| manifest.title.clone());
            let slug = slug.unwrap_or_else(|| {
                if manifest.feature.id.trim().is_empty() {
                    title.clone()
                } else {
                    manifest.feature.id.clone()
                }
            });
            let output = match output_html {
                Some(path) => user_story::write_storyboard_to(&path, &manifest, &title)?,
                None => user_story::write_storyboard(
                    std::path::Path::new(&repo.root_path),
                    &manifest,
                    &slug,
                    &title,
                )?,
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "output_html": output,
                    "manifest_embedded": true
                }))?
            );
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
            let summary = refresh_project_exports(
                &graph,
                &obsidian_output,
                &features_dir,
                all_features,
                &repo_root,
            )?;
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
        Commands::Communities {
            repo,
            resolution,
            top,
        } => {
            let storage = Storage::connect(&config.storage.database_url).await?;
            let repo = storage
                .find_repository(&repo)
                .await?
                .with_context(|| format!("repository is not indexed: {repo}"))?;
            let nodes = storage.load_all_nodes(repo.id).await?;
            let edges = storage.load_all_edges(repo.id).await?;
            let cfg = community::CommunityConfig { resolution };
            let mut detection = community::detect_communities(repo.id, &nodes, &edges, &cfg);
            // Largest communities first for human review; ties by id for stability.
            detection
                .communities
                .sort_by(|a, b| b.size.cmp(&a.size).then(a.id.cmp(&b.id)));
            if top > 0 {
                detection.communities.truncate(top);
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "repo_id": repo.id,
                    "repo": repo.name,
                    "resolution": detection.resolution,
                    "modularity": detection.modularity,
                    "levels": detection.levels,
                    "node_count": detection.node_count,
                    "edge_count": detection.edge_count,
                    "community_count": detection.communities.len(),
                    "quotient_edge_count": detection.quotient_edges.len(),
                    "communities": detection.communities,
                    "quotient_edges": detection.quotient_edges,
                }))?
            );
        }
        Commands::Mcp => {
            mcp::run(config).await?;
        }
        Commands::Setup { dry_run, scope } => {
            setup::run(cli.config.as_deref(), dry_run, scope)?;
        }
        // Hook is handled before this match — see the early-exit block above.
        // This arm satisfies exhaustiveness and is never reached at runtime.
        Commands::Hook { .. } => unreachable!("Hook handled by early-exit block"),
    }

    Ok(())
}
