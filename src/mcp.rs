use crate::{
    embedding::{build_embedder, Embedder},
    extractor::{current_commit, RustRepositoryExtractor},
    feature_context::{
        build_feature_context_warnings, load_feature_matches, write_feature_context_html,
        FeatureContextResponse,
    },
    feature_export::refresh_project_exports,
    obsidian_export::write_obsidian_vault,
    query::{query_feature_context_repo, query_repo},
    storage::Storage,
    Config,
};
use anyhow::{Context, Result};
use futures::{StreamExt, TryStreamExt};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, Write},
    path::{Path, PathBuf},
};

pub async fn run(config: Config) -> Result<()> {
    let storage = Storage::connect(&config.storage.database_url).await?;
    let embedder = build_embedder(&config.embedding)?;
    let mut stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    while let Some(message) = read_message(&mut stdin)? {
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "chaos-substrate", "version": env!("CARGO_PKG_VERSION")}
                }
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [
                        {
                            "name": "chaos_analyze",
                            "description": "Analyze and persist a repository knowledge graph and real embeddings. Replaces stale indexed data for that repository.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "repo_path": {"type": "string"}
                                },
                                "required": ["repo_path"]
                            }
                        },
                        {
                            "name": "chaos_add",
                            "description": "Incrementally index the files changed in git (or an explicit path list), refresh the Obsidian vault, and write an interactive feature/bug page into docs/features_memory — in one shot. Detects changes from the working tree by default (no file list needed); pass `since` for a committed range or `paths` to index specific files (code, Markdown/Notion exports, PDFs). Auto-classifies feature vs bug; override with `kind` and `message`.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "repo_path": {"type": "string", "description": "Repository to operate on. Defaults to the current directory."},
                                    "paths": {"type": "array", "items": {"type": "string"}, "description": "Explicit files to index; overrides git-diff detection."},
                                    "since": {"type": "string", "description": "Diff against this git ref instead of the working tree (e.g. HEAD~1, main)."},
                                    "kind": {"type": "string", "enum": ["feature", "bug"], "description": "Force the page classification. Auto-detected from git if omitted."},
                                    "message": {"type": "string", "description": "Short title/summary of the change; drives the page title and slug."},
                                    "obsidian_output": {"type": "string", "description": "Obsidian vault output directory. Defaults to <repo>/chaos-obsidian-vault."},
                                    "no_obsidian": {"type": "boolean", "default": false},
                                    "no_page": {"type": "boolean", "default": false}
                                },
                                "required": []
                            }
                        },
                        {
                            "name": "chaos_stats",
                            "description": "Report index statistics for an already-indexed repository, read from Postgres: totals (files, nodes, edges, chunks, embedded vs missing, split chunks) plus breakdowns of nodes by kind, edges by kind, chunks by type, and files by language. Read-only and embedder-free — use to explain or sanity-check what an analyze/add produced.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "repo": {"type": "string"}
                                },
                                "required": ["repo"]
                            }
                        },
                        {
                            "name": "chaos_query",
                            "description": "Query persisted code knowledge memory with hybrid semantic, keyword, and graph context routing. Set hierarchical=true for top-down retrieval: the query is matched against feature (L1 community) summaries first and the surfaced features are returned alongside chunk hits boosted toward them (falls back to flat search when the repo has no hierarchy).",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "repo": {"type": "string"},
                                    "question": {"type": "string"},
                                    "limit": {"type": "integer", "default": 10},
                                    "hierarchical": {"type": "boolean", "default": false, "description": "Route through matched features first (top-down), then drill into chunks."}
                                },
                                "required": ["repo", "question"]
                            }
                        },
                        {
                            "name": "chaos_feature_context",
                            "description": "Build focused implementation context for a feature or task. Reads Postgres retrieval plus generated feature-memory manifests and returns warnings when expected paths/docs are missing. Use this before composing any feature website; treat warnings as blockers before writing.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "repo": {"type": "string"},
                                    "task": {"type": "string"},
                                    "limit": {"type": "integer", "default": 10},
                                    "feature_limit": {"type": "integer", "default": 3},
                                    "nodes_per_feature": {"type": "integer", "default": 8},
                                    "features_dir": {"type": "string"},
                                    "output_html": {"type": "string"}
                                },
                                "required": ["repo", "task"]
                            }
                        },
                        {
                            "name": "chaos_impact",
                            "description": "Build a feature-vs-existing-code impact report for an indexed repo and ALWAYS write an interactive HTML (impact summary + evidence) into docs/features_memory. Returns a COMPACT summary — counts plus the existing files/symbols the feature touches, warnings, and the HTML path — and keeps the full evidence in the HTML only (so it won't flood your context like a raw feature_context dump). Use to see how a proposed feature maps onto the codebase as it exists today.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "repo": {"type": "string"},
                                    "feature": {"type": "string", "description": "The feature/task to assess (e.g. a spike doc's goal)."},
                                    "features_dir": {"type": "string"},
                                    "output_html": {"type": "string", "description": "Override the default docs/features_memory/<slug>-impact.html path."},
                                    "limit": {"type": "integer", "default": 10},
                                    "feature_limit": {"type": "integer", "default": 3},
                                    "nodes_per_feature": {"type": "integer", "default": 8}
                                },
                                "required": ["repo", "feature"]
                            }
                        },
                        {
                            "name": "chaos_write_feature_website",
                            "description": "Write an LLM-composed interactive feature website into docs/features_memory with an embedded chaos-feature-manifest JSON block. Use after chaos_feature_context, not as a substitute for understanding the feature. HTML must include interactive graph, story flow, architecture, code, and evidence sections.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "repo": {"type": "string"},
                                    "slug": {"type": "string"},
                                    "title": {"type": "string"},
                                    "html": {"type": "string"},
                                    "manifest": {"type": "object"}
                                },
                                "required": ["repo", "slug", "title", "html", "manifest"]
                            }
                        },
                        {
                            "name": "chaos_obsidian",
                            "description": "Export an already-indexed repository as an Obsidian vault (one Markdown note per graph node, grouped into topic notes, plus an edge manifest) read from the persisted graph. Run after chaos_analyze when you want browsable docs; chaos_analyze itself never writes files. Writes to <repo>/chaos-obsidian-vault by default.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "repo": {"type": "string"},
                                    "output": {"type": "string", "description": "Vault output directory. Defaults to <repo>/chaos-obsidian-vault."}
                                },
                                "required": ["repo"]
                            }
                        },
                        {
                            "name": "chaos_refresh",
                            "description": "Regenerate project-local artifacts from the persisted index without re-indexing: rewrites the Obsidian vault and, with all_features=true, re-renders the deterministic feature pages in docs/features_memory from their embedded manifests (refreshing each node's source snippet from the current repo). Run chaos_analyze or chaos_add first.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "repo": {"type": "string"},
                                    "obsidian_output": {"type": "string", "description": "Vault output directory. Defaults to <repo>/chaos-obsidian-vault."},
                                    "features_dir": {"type": "string", "description": "Feature-page directory. Defaults to <repo>/docs/features_memory."},
                                    "all_features": {"type": "boolean", "default": false, "description": "Also re-render every feature page from its embedded manifest."}
                                },
                                "required": ["repo"]
                            }
                        },
                        {
                            "name": "chaos_write_storyboard",
                            "description": "Write a CLIENT/USER-FACING interactive storyboard into docs/features_memory/<slug>-story.html: the feature explained from the UI/UX user-story perspective with NO code. You supply a structured, code-free manifest (personas; user stories as 'As a … I want … so that …' with plain-language acceptance criteria; clickable frames grouped into stages; outcomes; a confidence 0..1 on every frame/story/outcome plus an overall_confidence) and the tool renders a fixed dark Blade Runner page with click-a-frame detail and confidence rings. Use this for a stakeholder/end-user presentation; use chaos_write_feature_website instead for the engineer-facing graph/architecture/code page. Compose from real understanding (run chaos_feature_context / chaos_impact first); do not invent UI that does not exist.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "repo": {"type": "string"},
                                    "slug": {"type": "string", "description": "Slug for the output filename docs/features_memory/<slug>-story.html."},
                                    "title": {"type": "string", "description": "Page title; used when the manifest omits one."},
                                    "manifest": {"type": "object", "description": "StoryboardManifest: {title, subtitle, audience, overall_confidence, personas[], stories[], frames[], outcomes[]}. NO code/file/line fields. Minimums: >=1 persona, >=2 stories, >=3 frames, >=1 outcome; every confidence in [0,1]; story.frame_ids and persona references must resolve. Each frame MAY include an optional `preview` showing the REAL client UI (not code): {\"kind\":\"image\",\"src\":\"previews/x.png\",\"alt\":\"...\",\"caption\":\"...\"} for a screenshot/clip you captured (offline, leaks nothing — preferred), or {\"kind\":\"iframe\",\"url\":\"http://localhost:5173/route\",\"caption\":\"...\"} to live-embed a running app route (only renders while that server is up). src/url must not use javascript:/vbscript:/data:text/html."}
                                },
                                "required": ["repo", "slug", "title", "manifest"]
                            }
                        },
                        {
                            "name": "chaos_change_plan",
                            "description": "Decompose a proposed change into the FEATURES (L1 communities / god-nodes) it spans, with a dependency-aware check order — the top-down counterpart to flat retrieval. Matches the change description against community summary embeddings, optionally also seeding from a real git diff (`since`), then returns the set of features the change touches, each with its members, confidence, and a topo-sorted check order over the feature quotient graph. ALWAYS writes an interactive Blade-Runner HTML plan to docs/features_memory/<slug>-plan.html and returns a COMPACT JSON summary (counts + per-feature label/confidence/check_order/top symbols + the HTML path), so it won't flood your context. Use it to answer 'how many features does this change involve, and in what order should I check them?'. Requires the repo to be indexed (chaos_analyze/chaos_add build the hierarchy).",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "repo": {"type": "string"},
                                    "change_description": {"type": "string", "description": "Plain-language description of the change to scope."},
                                    "since": {"type": "string", "description": "Optional git ref (e.g. HEAD, main); also seeds the plan from the files actually changed vs this ref."},
                                    "output_html": {"type": "string", "description": "Override the default docs/features_memory/<slug>-plan.html path."},
                                    "limit": {"type": "integer", "default": 8, "description": "Max features to surface."}
                                },
                                "required": ["repo", "change_description"]
                            }
                        }
                    ]
                }
            }),
            "tools/call" => {
                let params = message.get("params").cloned().unwrap_or_default();
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match handle_tool_call(
                    name,
                    params.get("arguments").cloned().unwrap_or_default(),
                    &config,
                    &storage,
                    embedder.as_ref(),
                )
                .await
                {
                    Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Err(err) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "isError": true,
                            "content": [{"type": "text", "text": err.to_string()}]
                        }
                    }),
                }
            }
            "notifications/initialized" => continue,
            _ => json_error(id, -32601, "unknown method"),
        };
        write_message(&mut stdout, &response)?;
    }
    Ok(())
}

async fn handle_tool_call(
    name: &str,
    args: Value,
    config: &Config,
    storage: &Storage,
    embedder: &dyn Embedder,
) -> Result<Value> {
    match name {
        "chaos_analyze" => {
            let repo_path = args
                .get("repo_path")
                .and_then(Value::as_str)
                .context("repo_path is required")?;
            let summary = analyze_repo(config, storage, embedder, Path::new(repo_path)).await?;
            Ok(tool_text(summary))
        }
        "chaos_add" => {
            let repo_path = args.get("repo_path").and_then(Value::as_str).unwrap_or(".");
            let opts = crate::add::AddOptions {
                paths: args
                    .get("paths")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(PathBuf::from)
                            .collect()
                    })
                    .unwrap_or_default(),
                since: args.get("since").and_then(Value::as_str).map(String::from),
                kind: args.get("kind").and_then(Value::as_str).map(String::from),
                message: args
                    .get("message")
                    .and_then(Value::as_str)
                    .map(String::from),
                obsidian_output: args
                    .get("obsidian_output")
                    .and_then(Value::as_str)
                    .map(PathBuf::from),
                no_obsidian: args
                    .get("no_obsidian")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                no_page: args
                    .get("no_page")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            };
            let summary =
                crate::add::run(config, storage, embedder, Path::new(repo_path), &opts).await?;
            Ok(tool_text(serde_json::to_string_pretty(&summary)?))
        }
        "chaos_stats" => {
            let repo = args
                .get("repo")
                .and_then(Value::as_str)
                .context("repo is required")?;
            let repo = storage
                .find_repository(repo)
                .await?
                .context("repository is not indexed")?;
            let stats = storage.repo_stats(&repo).await?;
            Ok(tool_text(serde_json::to_string_pretty(&stats)?))
        }
        "chaos_query" => {
            let repo = args
                .get("repo")
                .and_then(Value::as_str)
                .context("repo is required")?;
            let question = args
                .get("question")
                .and_then(Value::as_str)
                .context("question is required")?;
            let limit = args.get("limit").and_then(Value::as_i64).unwrap_or(10);
            let hierarchical = args
                .get("hierarchical")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let repo = storage
                .find_repository(repo)
                .await?
                .context("repository is not indexed")?;
            if hierarchical {
                let answer = crate::query::query_repo_hierarchical(
                    storage, repo.id, embedder, question, limit,
                )
                .await?;
                Ok(tool_text(serde_json::to_string_pretty(&answer)?))
            } else {
                let answer = query_repo(storage, repo.id, embedder, question, limit).await?;
                Ok(tool_text(serde_json::to_string_pretty(&answer)?))
            }
        }
        "chaos_feature_context" => {
            let repo = args
                .get("repo")
                .and_then(Value::as_str)
                .context("repo is required")?;
            let task = args
                .get("task")
                .and_then(Value::as_str)
                .context("task is required")?;
            let limit = args.get("limit").and_then(Value::as_i64).unwrap_or(10);
            let feature_limit = args
                .get("feature_limit")
                .and_then(Value::as_u64)
                .unwrap_or(3) as usize;
            let nodes_per_feature = args
                .get("nodes_per_feature")
                .and_then(Value::as_u64)
                .unwrap_or(8) as usize;
            let repo = storage
                .find_repository(repo)
                .await?
                .context("repository is not indexed")?;
            let repo_root = PathBuf::from(&repo.root_path);
            let features_dir = args
                .get("features_dir")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(|| repo_root.join("docs/features_memory"));
            let postgres =
                query_feature_context_repo(storage, repo.id, embedder, task, limit).await?;
            let warnings = build_feature_context_warnings(task, &repo_root, &postgres);
            let feature_matches =
                load_feature_matches(task, &features_dir, feature_limit, nodes_per_feature)?;
            let response = FeatureContextResponse {
                task: task.to_string(),
                postgres,
                features_dir,
                warnings,
                feature_matches,
            };
            let output_html = args.get("output_html").and_then(Value::as_str);
            if let Some(output_html) = output_html {
                write_feature_context_html(Path::new(output_html), &response)?;
            }
            Ok(tool_text(serde_json::to_string_pretty(&json!({
                "wrote_html": output_html,
                "context": response
            }))?))
        }
        "chaos_impact" => {
            let repo = args
                .get("repo")
                .and_then(Value::as_str)
                .context("repo is required")?;
            let feature = args
                .get("feature")
                .and_then(Value::as_str)
                .context("feature is required")?;
            let opts = crate::impact::ImpactOptions {
                features_dir: args
                    .get("features_dir")
                    .and_then(Value::as_str)
                    .map(PathBuf::from),
                output_html: args
                    .get("output_html")
                    .and_then(Value::as_str)
                    .map(PathBuf::from),
                limit: args.get("limit").and_then(Value::as_i64).unwrap_or(10),
                feature_limit: args
                    .get("feature_limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(3) as usize,
                nodes_per_feature: args
                    .get("nodes_per_feature")
                    .and_then(Value::as_u64)
                    .unwrap_or(8) as usize,
            };
            let summary = crate::impact::run(storage, embedder, repo, feature, &opts).await?;
            Ok(tool_text(serde_json::to_string_pretty(&summary)?))
        }
        "chaos_write_feature_website" => {
            let repo = args
                .get("repo")
                .and_then(Value::as_str)
                .context("repo is required")?;
            let slug = args
                .get("slug")
                .and_then(Value::as_str)
                .context("slug is required")?;
            let title = args
                .get("title")
                .and_then(Value::as_str)
                .context("title is required")?;
            let html = args
                .get("html")
                .and_then(Value::as_str)
                .context("html is required")?;
            let manifest = args.get("manifest").context("manifest is required")?;
            let repo = storage
                .find_repository(repo)
                .await?
                .context("repository is not indexed")?;
            let path = write_llm_feature_website(&repo.root_path, slug, title, html, manifest)?;
            Ok(tool_text(serde_json::to_string_pretty(&json!({
                "output_html": path,
                "manifest_embedded": true
            }))?))
        }
        "chaos_obsidian" => {
            let repo = args
                .get("repo")
                .and_then(Value::as_str)
                .context("repo is required")?;
            let repo = storage
                .find_repository(repo)
                .await?
                .context("repository is not indexed")?;
            let repo_root = PathBuf::from(&repo.root_path);
            let output = args
                .get("output")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(|| repo_root.join("chaos-obsidian-vault"));
            let graph = storage.load_graph_export(&repo).await?;
            let summary = write_obsidian_vault(&output, &graph)?;
            Ok(tool_text(serde_json::to_string_pretty(&json!({
                "output": summary.output,
                "repo_id": repo.id,
                "topics": summary.topics,
                "node_notes": summary.node_notes,
                "edges": summary.edges
            }))?))
        }
        "chaos_refresh" => {
            let repo = args
                .get("repo")
                .and_then(Value::as_str)
                .context("repo is required")?;
            let repo = storage
                .find_repository(repo)
                .await?
                .context("repository is not indexed")?;
            let repo_root = PathBuf::from(&repo.root_path);
            let obsidian_output = args
                .get("obsidian_output")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(|| repo_root.join("chaos-obsidian-vault"));
            let features_dir = args
                .get("features_dir")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(|| repo_root.join("docs/features_memory"));
            let all_features = args
                .get("all_features")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let graph = storage.load_graph_export(&repo).await?;
            let summary = refresh_project_exports(
                &graph,
                &obsidian_output,
                &features_dir,
                all_features,
                &repo_root,
            )?;
            Ok(tool_text(serde_json::to_string_pretty(&json!({
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
            }))?))
        }
        "chaos_write_storyboard" => {
            let repo = args
                .get("repo")
                .and_then(Value::as_str)
                .context("repo is required")?;
            let slug = args
                .get("slug")
                .and_then(Value::as_str)
                .context("slug is required")?;
            let title = args
                .get("title")
                .and_then(Value::as_str)
                .context("title is required")?;
            let manifest_value = args.get("manifest").context("manifest is required")?;
            let manifest: crate::user_story::StoryboardManifest = serde_json::from_value(
                manifest_value.clone(),
            )
            .context(
                "manifest must match the storyboard schema (personas, stories, frames, outcomes)",
            )?;
            let repo = storage
                .find_repository(repo)
                .await?
                .context("repository is not indexed")?;
            let path = crate::user_story::write_storyboard(
                Path::new(&repo.root_path),
                &manifest,
                slug,
                title,
            )?;
            Ok(tool_text(serde_json::to_string_pretty(&json!({
                "output_html": path,
                "manifest_embedded": true
            }))?))
        }
        "chaos_change_plan" => {
            let repo = args
                .get("repo")
                .and_then(Value::as_str)
                .context("repo is required")?;
            let change = args
                .get("change_description")
                .and_then(Value::as_str)
                .context("change_description is required")?;
            let opts = crate::change_plan::ChangePlanOptions {
                output_html: args
                    .get("output_html")
                    .and_then(Value::as_str)
                    .map(PathBuf::from),
                diff_since: args.get("since").and_then(Value::as_str).map(String::from),
                limit: args.get("limit").and_then(Value::as_u64).unwrap_or(8) as usize,
            };
            let summary = crate::change_plan::run(storage, embedder, repo, change, &opts).await?;
            Ok(tool_text(serde_json::to_string_pretty(&summary)?))
        }
        _ => anyhow::bail!("unknown tool: {name}"),
    }
}

fn write_llm_feature_website(
    repo_root: &str,
    slug: &str,
    title: &str,
    html: &str,
    manifest: &Value,
) -> Result<PathBuf> {
    let slug = safe_slug(slug);
    let output = Path::new(repo_root)
        .join("docs/features_memory")
        .join(format!("{slug}-explanation.html"));
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let manifest_json = serde_json::to_string_pretty(manifest)?;
    let manifest_block = format!(
        r#"<script type="application/json" id="chaos-feature-manifest">
{}
</script>"#,
        escape_script_json_for_html(&manifest_json)
    );
    if html.contains("id=\"chaos-feature-manifest\"")
        || html.contains("id='chaos-feature-manifest'")
    {
        anyhow::bail!(
            "html must not include chaos-feature-manifest; pass the manifest argument and the tool will embed it"
        );
    }
    validate_feature_website_contract(html, manifest)?;
    let page = format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{}</title>
</head>
<body>
{}
{}
</body>
</html>
"#,
        escape_html(title),
        html,
        manifest_block
    );
    fs::write(&output, page)?;
    Ok(output)
}

pub(crate) fn validate_feature_website_contract(html: &str, manifest: &Value) -> Result<()> {
    let required_manifest = [
        ("claims", 3usize),
        ("modes", 2usize),
        ("nodes", 5usize),
        ("edges", 3usize),
        ("story", 3usize),
    ];
    for (field, minimum) in required_manifest {
        let count = manifest
            .get(field)
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        if count < minimum {
            anyhow::bail!(
                "manifest.{field} must contain at least {minimum} items for an evidence-backed feature website; got {count}"
            );
        }
    }

    let required_html_markers = [
        "data-chaos-feature-website",
        "data-chaos-graph",
        "data-node-id",
        "data-chaos-story",
        "data-story-step",
        "data-chaos-architecture",
        "data-chaos-flow",
        "data-chaos-code",
        "data-chaos-evidence",
    ];
    for marker in required_html_markers {
        if !html.contains(marker) {
            anyhow::bail!("html is missing required interactive feature website marker `{marker}`");
        }
    }

    let lowercase = html.to_ascii_lowercase();
    if !lowercase.contains("<script") || !html.contains("addEventListener") {
        anyhow::bail!(
            "html must include JavaScript interactivity with event listeners for graph/story/code navigation"
        );
    }

    Ok(())
}

fn safe_slug(input: &str) -> String {
    let slug = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "feature-context".to_string()
    } else {
        slug
    }
}

fn escape_script_json_for_html(json: &str) -> String {
    json.replace("</script", "<\\/script")
}

fn escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

async fn analyze_repo(
    config: &Config,
    storage: &Storage,
    embedder: &dyn Embedder,
    repo_path: &Path,
) -> Result<String> {
    let commit = current_commit(repo_path);
    let repo = storage
        .upsert_repository(repo_path, commit.as_deref())
        .await?;
    let run_id = storage.begin_analysis(repo.id, commit.as_deref()).await?;
    let outcome = async {
        let extractor = RustRepositoryExtractor::new(config.indexing.clone());
        let result = extractor.extract(repo_path, repo.id, commit)?;
        storage.replace_repo_index(repo.id, &result).await?;
        let missing = storage
            .chunks_missing_embeddings(
                repo.id,
                embedder.provider(),
                embedder.model_id(),
                embedder.dimensions(),
            )
            .await?;
        futures::stream::iter(missing.iter().map(|chunk| async move {
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
            Result::<_, anyhow::Error>::Ok(())
        }))
        .buffer_unordered(crate::EMBED_CONCURRENCY)
        .try_collect::<()>()
        .await?;
        // L1: derive + persist the community layer from the written graph.
        let detection = crate::community::detect_and_persist(
            storage,
            repo.id,
            &crate::community::CommunityConfig::default(),
        )
        .await?;
        // L2: roll the content-hash leaves up to file/community/repo roots.
        let merkle = crate::merkle::compute_and_persist(storage, repo.id).await?;
        // L3: hash-gated community summaries, embedded by the real embedder.
        let summary = crate::community_summary::summarize_repo(storage, embedder, repo.id).await?;
        let feature_communities = detection.communities.iter().filter(|c| c.size >= 2).count();
        Result::<_, anyhow::Error>::Ok(json!({
            "repo_id": repo.id,
            "files": result.files.len(),
            "nodes": result.nodes.len(),
            "edges": result.edges.len(),
            "chunks": result.chunks.len(),
            "embedded_chunks": missing.len(),
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
        }))
    }
    .await;

    match outcome {
        Ok(summary) => {
            storage.finish_analysis(run_id, "completed", None).await?;
            Ok(serde_json::to_string_pretty(&summary)?)
        }
        Err(err) => {
            storage
                .finish_analysis(run_id, "failed", Some(&err.to_string()))
                .await?;
            Err(err)
        }
    }
}

fn tool_text(text: String) -> Value {
    json!({"content": [{"type": "text", "text": text}]})
}

fn json_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn read_message(stdin: &mut std::io::Stdin) -> Result<Option<Value>> {
    // Skip blank keep-alive lines iteratively. A recursive call here would let
    // a client streaming many empty lines overflow the stack (DoS), so loop.
    loop {
        let mut line = String::new();
        let bytes_read = stdin.lock().read_line(&mut line)?;
        if bytes_read == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            continue;
        }
        return Ok(Some(serde_json::from_str(trimmed)?));
    }
}

fn write_message(stdout: &mut std::io::Stdout, message: &Value) -> Result<()> {
    let body = serde_json::to_string(message)?;
    stdout.write_all(body.as_bytes())?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_manifest() -> Value {
        json!({
            "claims": [{}, {}, {}],
            "modes": [{}, {}],
            "nodes": [{}, {}, {}, {}, {}],
            "edges": [{}, {}, {}],
            "story": [{}, {}, {}]
        })
    }

    #[test]
    fn feature_website_contract_rejects_readme_like_html() {
        let err = validate_feature_website_contract(
            "<section><h1>Feature</h1></section>",
            &valid_manifest(),
        )
        .expect_err("plain prose should not pass as a feature website");
        assert!(err.to_string().contains("data-chaos-feature-website"));
    }

    #[test]
    fn feature_website_contract_accepts_interactive_surface() {
        let html = r#"
          <main data-chaos-feature-website>
            <section data-chaos-architecture></section>
            <section data-chaos-flow></section>
            <svg data-chaos-graph><g data-node-id="a"></g></svg>
            <ol data-chaos-story><li data-story-step="one"></li></ol>
            <pre data-chaos-code></pre>
            <aside data-chaos-evidence></aside>
          </main>
          <script>document.querySelector('[data-node-id]').addEventListener('click', () => {});</script>
        "#;
        validate_feature_website_contract(html, &valid_manifest()).unwrap();
    }
}
