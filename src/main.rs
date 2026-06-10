mod add;
mod change_plan;
mod community;
mod community_summary;
mod components;
mod config;
mod embedding;
mod export_util;
mod extractor;
mod feature_context;
mod feature_export;
mod feature_inventory;
mod graph;
mod graph_export;
mod hierarchy_export;
mod hook;
mod impact;
mod lang;
mod layering;
mod linker;
mod mcp;
mod merkle;
mod models;
mod obsidian_export;
mod project;
mod provenance;
mod query;
mod setup;
mod simple_graph_optimizer;
mod storage;
mod struct_features;
mod theme;
mod user_story;
mod weights;

pub use config::Config;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use embedding::build_embedder;
use extractor::{current_commit, RustRepositoryExtractor};
use feature_context::{
    build_feature_context_warnings, feature_context_provenance, load_feature_matches,
    write_feature_context_html, FeatureContextResponse,
};
use feature_export::refresh_project_exports;
use graph_export::write_graph_html;
use obsidian_export::write_obsidian_vault;
use query::{query_feature_context_repo, query_repo};
use serde_json::json;
use std::path::PathBuf;
use storage::Storage;
use tracing::warn;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "chaos")]
#[command(about = "Persistent code knowledge memory for agents")]
// `chaos help` is our agent-oriented guide (see [`print_agent_help`]), not
// clap's built-in alias for `--help` — the flag form still works everywhere.
#[command(disable_help_subcommand = true)]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Agent-friendly guide: every command with what it's for, typical
    /// workflows, and copy-paste examples. Needs no database or config — safe
    /// to run from anywhere. `chaos help <command>` shows that command's full
    /// flags.
    Help {
        /// Optional command name for focused help (e.g. `chaos help analyze`).
        command: Option<String>,
    },
    /// Apply database migrations.
    Migrate,
    /// Verify database and embedder configuration.
    Doctor,
    /// Wipe the persisted index. With no argument clears every repository;
    /// pass a repo path/name to clear only that repository. The database is
    /// always wiped; pass --artifacts to ALSO delete the generated files on
    /// disk (chaos-obsidian-vault/, docs/features_memory/, and — when clearing
    /// everything — the project workspaces under ~/.chaos/projects), so a
    /// validation run can start truly clean.
    Clean {
        /// Optional repository path or name to clear; omit to clear everything.
        repo: Option<String>,
        /// Also delete generated artifacts from disk (vault, feature pages,
        /// project workspaces). Off by default — feature pages are often
        /// committed to git as durable feature memory.
        #[arg(long)]
        artifacts: bool,
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
    /// Explain the CORE COMPONENTS of a big area (the step before feature
    /// extraction). Given an area like "OCL" (or nothing, for a repo-level
    /// overview) it surfaces the communities that make up the area as
    /// components, shows how they connect, and proposes a read order — then
    /// writes an interactive HTML overview into docs/features_memory and prints
    /// a compact JSON summary.
    Components {
        repo: String,
        /// Area/subsystem to explain (e.g. "OCL"). Omit for a repo-level overview.
        area: Option<String>,
        /// Override the default docs/features_memory/<slug>-components.html path.
        #[arg(long)]
        output_html: Option<PathBuf>,
        /// Max components to surface.
        #[arg(long, default_value_t = 8)]
        limit: usize,
        /// Representative members (symbols/files) loaded per component.
        #[arg(long, default_value_t = 12)]
        top_members: usize,
    },
    /// List ALL god-node features (L1 communities), grouped by journey layer.
    /// The optional filter is auto-detected: a path / real directory → folder
    /// scope; a layer word (client/ui/api/core/contracts) → that journey layer;
    /// any other phrase is first tried as a layer BY MEANING (embedding match —
    /// "backend", "client app", "devops" resolve semantically; "backend" spans
    /// interface+core) and then falls to a topic match; nothing → the whole
    /// repo. Forces available via --layer/--folder/--topic. Writes an
    /// interactive HTML inventory into docs/features_memory and prints a
    /// compact JSON summary. Exhaustive (no curation) — the counterpart to
    /// `components`' ordered overview.
    Features {
        /// Repository to list (omit when using --project).
        repo: Option<String>,
        /// Optional filter, auto-detected as folder / layer / topic.
        filter: Option<String>,
        /// List features across ALL repos of this project instead of one repo
        /// (cards are tagged with repo aliases and cross-repo links; the HTML
        /// goes to the project workspace).
        #[arg(long)]
        project: Option<String>,
        /// Force a layer filter (entry|interface|core|foundation, or a synonym
        /// like client/api/contracts).
        #[arg(long)]
        layer: Option<String>,
        /// Force a folder filter (features with code under this path).
        #[arg(long)]
        folder: Option<String>,
        /// Force a topic (semantic + keyword) filter.
        #[arg(long)]
        topic: Option<String>,
        /// Override the default docs/features_memory/<slug>-features.html path.
        #[arg(long)]
        output_html: Option<PathBuf>,
        /// Cap features surfaced; 0 = all (the default — this listing is exhaustive).
        #[arg(long, default_value_t = 0)]
        limit: usize,
        /// JSON file with agent-written domain curation ({"groups": [{"title",
        /// "icon"?, "blurb"?, "features": [{"label", "note"?}]}]}) — re-renders
        /// the SAME inventory page with those human domains/notes as the
        /// primary sections.
        #[arg(long)]
        curation: Option<PathBuf>,
    },
    /// Manage cross-repository projects: group indexed repos (client, backend,
    /// contracts, infra, …) under one name and maintain feature→feature
    /// cross-repo links between them. Links refresh automatically after
    /// analyze/add (hash-gated), so the project layer follows the same layered
    /// pipeline as L1–L3.
    Project {
        #[command(subcommand)]
        action: ProjectAction,
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
        /// Apply a shipped brand preset by name (e.g. "molecule") — fills the
        /// logo/hero/company for any fields the manifest leaves empty.
        #[arg(long)]
        brand_preset: Option<String>,
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
    /// Debug-only: PROTOTYPE structure-first feature extraction over a folder,
    /// printed side-by-side with the current Louvain communities. Read-only; runs
    /// off the existing index (no re-analyze). A spike to evaluate deriving
    /// features from project structure instead of import-graph clustering.
    #[command(hide = true)]
    StructFeatures {
        repo: String,
        /// Folder to analyze (e.g. `desci-infra`, `desci-ecosystem`).
        folder: String,
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

#[derive(Subcommand)]
enum ProjectAction {
    /// Create a project (idempotent).
    Create { name: String },
    /// Attach an already-indexed repository to a project and link it against
    /// the existing members.
    AddRepo {
        project: String,
        repo: String,
        /// Project-scoped alias (client/backend/contracts/infra/…). Defaults
        /// to the repository name.
        #[arg(long)]
        alias: Option<String>,
    },
    /// List all projects with their member repos and link counts, plus EVERY
    /// indexed repository — the discovery call when you don't know what Chaos
    /// already knows.
    List,
    /// Show a project's members, link staleness, links by kind, and embedder
    /// consistency.
    Status { project: String },
    /// Re-detect cross-repo links (hash-gated; --force overrides the gate).
    Relink {
        project: String,
        #[arg(long)]
        force: bool,
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

    // `help` must work from any directory with no config, database, or
    // embedder — it's how agents discover the tool, often before any setup.
    if let Commands::Help { ref command } = cli.command {
        print_agent_help(command.as_deref())?;
        return Ok(());
    }

    let config = Config::load(cli.config.as_deref())?;

    match cli.command {
        Commands::Help { .. } => unreachable!("Help handled by early-exit block"),
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
        Commands::Clean { repo, artifacts } => {
            let storage = Storage::connect(&config.storage.database_url).await?;
            let summary = run_clean(&storage, repo.as_deref(), artifacts).await?;
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
                embedding::embed_missing_chunks(&storage, embedder.as_ref(), &missing).await?;
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
                Result::<_, anyhow::Error>::Ok((
                    result,
                    reused,
                    missing.len(),
                    detection,
                    merkle,
                    summary,
                ))
            }
            .await;

            match outcome {
                Ok((result, reused_embeddings, embedded, detection, merkle, summary)) => {
                    storage.finish_analysis(run_id, "completed", None).await?;
                    // P6: keep the project layer fresh — relink every project
                    // containing this repo (hash-gated; empty when none).
                    let projects = project::relink_projects_for_repo(&storage, repo.id).await;
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
                let mut response =
                    query_repo(&storage, repo.id, embedder.as_ref(), &question, limit).await?;
                // Return-only surface: excerpt chunk contents.
                query::cap_hits_for_return(&mut response.hits);
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
            let provenance = feature_context_provenance(&postgres, &features_dir, &feature_matches);
            let mut response = FeatureContextResponse {
                task,
                postgres,
                features_dir,
                warnings,
                feature_matches,
                provenance,
            };
            if let Some(output_html) = output_html {
                // The HTML keeps the FULL evidence; the printed JSON gets excerpts.
                write_feature_context_html(&output_html, &response)?;
            }
            feature_context::cap_response_for_return(&mut response);
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
        Commands::Components {
            repo,
            area,
            output_html,
            limit,
            top_members,
        } => {
            let storage = Storage::connect(&config.storage.database_url).await?;
            let embedder = build_embedder(&config.embedding)?;
            let opts = components::ComponentsOptions {
                output_html,
                limit,
                top_members,
            };
            let summary =
                components::run(&storage, embedder.as_ref(), &repo, area.as_deref(), &opts).await?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        Commands::Features {
            repo,
            filter,
            project,
            layer,
            folder,
            topic,
            output_html,
            limit,
            curation,
        } => {
            let storage = Storage::connect(&config.storage.database_url).await?;
            // Only a topic filter needs the embedder; layer/folder/whole-repo
            // listing stays embedder-free, so a missing/misconfigured embedder
            // should not block them (it just degrades a topic match to keywords).
            let embedder = build_embedder(&config.embedding).ok();
            let curation = curation
                .map(|p| -> anyhow::Result<feature_inventory::CurationSpec> {
                    let raw = std::fs::read_to_string(&p)
                        .with_context(|| format!("reading curation file {}", p.display()))?;
                    serde_json::from_str(&raw)
                        .with_context(|| format!("parsing curation file {}", p.display()))
                })
                .transpose()?;
            let opts = feature_inventory::FeatureInventoryOptions {
                output_html,
                limit,
                layer,
                folder,
                topic,
                curation,
            };
            let summary = match (&project, &repo) {
                (Some(project), _) => {
                    feature_inventory::run_project(
                        &storage,
                        embedder.as_deref(),
                        project,
                        filter.as_deref(),
                        &opts,
                    )
                    .await?
                }
                (None, Some(repo)) => {
                    feature_inventory::run(
                        &storage,
                        embedder.as_deref(),
                        repo,
                        filter.as_deref(),
                        &opts,
                    )
                    .await?
                }
                (None, None) => anyhow::bail!("pass a repo, or --project <name>"),
            };
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        Commands::Project { action } => {
            let storage = Storage::connect(&config.storage.database_url).await?;
            let summary = match action {
                ProjectAction::Create { name } => project::create(&storage, &name).await?,
                ProjectAction::AddRepo {
                    project,
                    repo,
                    alias,
                } => project::add_repo(&storage, &project, &repo, alias.as_deref()).await?,
                ProjectAction::List => project::list(&storage).await?,
                ProjectAction::Status { project } => project::status(&storage, &project).await?,
                ProjectAction::Relink { project, force } => {
                    project::relink(&storage, &project, force).await?
                }
            };
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        Commands::Storyboard {
            repo,
            manifest,
            output_html,
            slug,
            title,
            brand_preset,
        } => {
            let storage = Storage::connect(&config.storage.database_url).await?;
            let repo = storage
                .find_repository(&repo)
                .await?
                .with_context(|| format!("repository is not indexed: {repo}"))?;
            let raw = std::fs::read_to_string(&manifest)
                .with_context(|| format!("reading storyboard manifest {}", manifest.display()))?;
            let mut manifest: user_story::StoryboardManifest =
                serde_json::from_str(&raw).context("parsing storyboard manifest JSON")?;
            if let Some(preset) = brand_preset {
                manifest.brand_preset = preset;
            }
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
            let hierarchy = storage.load_community_hierarchy(&repo, 14).await?;
            let hier = hierarchy_export::write_hierarchy(&output, &output, &hierarchy)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "output": summary.output,
                    "repo_id": repo.id,
                    "topics": summary.topics,
                    "node_notes": summary.node_notes,
                    "edges": summary.edges,
                    "community_notes": hier.community_notes,
                    "feature_map_html": hier.feature_map_html
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
            let hierarchy = storage.load_community_hierarchy(&repo, 14).await?;
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
                Some(&hierarchy),
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
                    "skipped_feature_pages": summary.skipped_feature_pages,
                    "community_notes": summary.community_notes,
                    "feature_map_html": summary.feature_map_html
                }))?
            );
        }
        Commands::StructFeatures { repo, folder } => {
            let storage = Storage::connect(&config.storage.database_url).await?;
            struct_features::run(&storage, &repo, &folder).await?;
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

/// The `chaos help` guide. The command list is generated from clap's own
/// metadata (names + first doc line), so it can never drift from the real
/// CLI; only the workflow examples below it are curated.
fn print_agent_help(topic: Option<&str>) -> Result<()> {
    use clap::CommandFactory;
    let mut root = Cli::command();

    if let Some(topic) = topic {
        let Some(sub) = root.find_subcommand_mut(topic) else {
            anyhow::bail!("unknown command `{topic}` — run `chaos help` for the full list");
        };
        print!("{}", sub.render_long_help());
        return Ok(());
    }

    println!(
        "Chaos Substrate {} — persistent code knowledge memory for agents\n",
        env!("CARGO_PKG_VERSION")
    );
    println!("USAGE: chaos [--config <PATH>] <command> [args]\n");
    println!(
        "When this repo is registered as an MCP server, prefer the chaos_* MCP tools —\n\
         same engine, structured returns. This CLI is the standalone surface.\n"
    );

    println!("COMMANDS");
    for sub in root.get_subcommands() {
        if sub.is_hide_set() {
            continue;
        }
        let about = sub.get_about().map(|a| a.to_string()).unwrap_or_default();
        // First sentence only — the full text is one `chaos help <command>` away.
        let brief = match about.find(". ") {
            Some(i) => &about[..i + 1],
            None => about.as_str(),
        };
        println!("  {:<18} {brief}", sub.get_name());
    }

    println!(
        "\nTYPICAL WORKFLOWS\n\
         \x20 first index       chaos migrate && chaos doctor && chaos analyze /path/to/repo\n\
         \x20 after editing     chaos add /path/to/repo -m \"what changed\"\n\
         \x20 ask a question    chaos query /path/to/repo \"how does auth work?\" --hierarchical\n\
         \x20 grasp a big area  chaos components /path/to/repo \"payments\"\n\
         \x20 list features     chaos features /path/to/repo client\n\
         \x20 scope a change    chaos change-plan /path/to/repo \"add rate limiting\" --since main\n\
         \x20 cross-repo        chaos project create app && chaos project add-repo app /repo --alias backend\n\
         \x20                   chaos features --project app\n\
         \x20 fresh start       chaos clean --artifacts\n\
         \nDETAILS\n\
         \x20 chaos help <command>   full flags for one command (same as chaos <command> --help)\n\
         \x20 RUNBOOK.md             complete ops reference · README.md  MCP tool reference\n\
         \x20 Config: chaos-substrate.toml or DATABASE_URL/CHAOS_EMBED_* env. A real embedder\n\
         \x20 (OpenAI/Ollama) is required for analyze/add/query; never fake vectors."
    );
    Ok(())
}

/// Wipe the persisted index — one repository, or everything — optionally
/// deleting the generated files on disk too. Shared by the `chaos clean` CLI
/// arm and the `chaos_clean` MCP tool.
pub(crate) async fn run_clean(
    storage: &Storage,
    repo: Option<&str>,
    artifacts: bool,
) -> Result<serde_json::Value> {
    if let Some(repo) = repo {
        let repository = storage
            .find_repository(repo)
            .await?
            .with_context(|| format!("repository is not indexed: {repo}"))?;
        let removed_artifacts = if artifacts {
            remove_generated_artifacts(std::path::Path::new(&repository.root_path))
        } else {
            Vec::new()
        };
        storage.purge_repository(repository.id).await?;
        Ok(json!({
            "cleared": "repository",
            "root_path": repository.root_path,
            "repo_id": repository.id,
            "artifacts_removed": removed_artifacts,
        }))
    } else {
        // Collect artifact locations BEFORE the wipe (the rows are the only
        // record of where the repos and project workspaces live).
        let mut removed_artifacts: Vec<String> = Vec::new();
        if artifacts {
            for repository in storage.list_repositories().await? {
                removed_artifacts.extend(remove_generated_artifacts(std::path::Path::new(
                    &repository.root_path,
                )));
            }
            for proj in storage.list_projects().await? {
                let workspace = project::project_workspace_dir(&proj.name);
                if workspace.is_dir() && std::fs::remove_dir_all(&workspace).is_ok() {
                    removed_artifacts.push(workspace.display().to_string());
                }
            }
        }
        let removed = storage.clear_all().await?;
        Ok(json!({
            "cleared": "all",
            "removed": removed,
            "artifacts_removed": removed_artifacts,
        }))
    }
}

/// Delete the generated artifacts Chaos writes inside a repository — exactly
/// the two deterministic homes (`chaos-obsidian-vault/` and
/// `docs/features_memory/`), never anything else. Returns the paths actually
/// removed. Exports written to caller-chosen paths (`graph -o`, explicit
/// `--output-html`) are not tracked and must be removed by hand.
fn remove_generated_artifacts(repo_root: &std::path::Path) -> Vec<String> {
    let mut removed = Vec::new();
    for dir in [
        repo_root.join("chaos-obsidian-vault"),
        repo_root.join("docs/features_memory"),
    ] {
        if dir.is_dir() && std::fs::remove_dir_all(&dir).is_ok() {
            removed.push(dir.display().to_string());
        }
    }
    removed
}
