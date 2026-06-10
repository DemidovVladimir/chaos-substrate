use crate::{
    embedding::{build_embedder, Embedder},
    extractor::{current_commit, RustRepositoryExtractor},
    feature_context::{
        build_feature_context_warnings, feature_context_provenance, load_feature_matches,
        write_feature_context_html, FeatureContextResponse,
    },
    feature_export::refresh_project_exports,
    obsidian_export::write_obsidian_vault,
    query::{query_feature_context_repo, query_repo},
    storage::Storage,
    Config,
};
use anyhow::{Context, Result};
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
                    "serverInfo": {"name": "chaos-substrate", "version": env!("CARGO_PKG_VERSION")},
                    // Kept deliberately tiny — it loads into every session.
                    // The full workflow guide is one chaos_help call away.
                    "instructions": "Persistent code knowledge memory. Index with chaos_analyze (full) or chaos_add (after edits); ask with chaos_query (hierarchical=true for feature routing); orient with chaos_components / chaos_features; tech stack & infra via chaos_stack; scope changes with chaos_change_plan; cross-repo via chaos_project. Tool returns are compact excerpts — full evidence lives in the generated HTML pages. Call chaos_help for workflows and tool order."
                }
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [
                        {
                            "name": "chaos_help",
                            "description": "The agent guide for this server: recommended tool ORDER and WORKFLOWS (first index, incremental updates, asking questions, orienting in a big codebase, scoping a change, documenting a feature, cross-repo projects, starting clean), plus token notes (returns are excerpts; HTML pages keep full evidence). Costs nothing until called — use it once when you first meet this server, or whenever unsure which tool fits.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {},
                                "required": []
                            }
                        },
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
                            "description": "Incrementally index the files changed in git (or an explicit path list), refresh the Obsidian vault, and write an interactive feature/bug page into docs/features_memory — in one shot. Detects changes from the working tree by default (no file list needed); pass `since` for a committed range or `paths` to index specific files (code, Markdown/Notion exports, PDFs). Auto-classifies feature vs bug; override with `kind` and `message`. The page records PROVENANCE breadcrumbs (how it was generated: git diff, AST/language extraction, Postgres graph load, file reads, manifest correlation) plus per-node evidence, and CORRELATES the change with previously generated feature pages by shared files/symbols (surfaced as related_features + a correlation claim) so a new extraction understands the existing features it overlaps.",
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
                            "name": "chaos_stack",
                            "description": "Report the TECH STACK of an already-indexed repository, LISTED rather than counted (chaos_stats only counts these node kinds): manifest-DECLARED dependencies by ecosystem (npm/cargo — name, versions, runtime-vs-dev scope, and how many workspace manifests declare each, widest-declared first), npm scripts, deployment resources (AWS CDK app entrypoints, Stack classes, and L2 constructs grouped by cloud service), indexed JS/TS configs, and the file-language breakdown. Read-only and embedder-free. ALWAYS writes an interactive HTML inventory to docs/features_memory/stack.html (manifest embedded under id=\"chaos-stack-manifest\") and returns a COMPACT JSON summary (capped lists with *_omitted counts; every entry lives in the HTML). The return states its COVERAGE explicitly — what the index extracts vs what it does not yet (Dockerfiles, CI workflows, pyproject.toml, foundry.toml, Terraform), so read those files directly if they matter. Use it to answer \"what is this repo built with / what infrastructure does it use?\" without grepping manifests.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "repo": {"type": "string"},
                                    "output_html": {"type": "string", "description": "Override the default docs/features_memory/stack.html path."}
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
                            "description": "Build focused implementation context for a feature or task. Reads Postgres retrieval plus generated feature-memory manifests and returns warnings when expected paths/docs are missing. Use this before composing any feature website; treat warnings as blockers before writing. Each retrieval hit is tagged with its retrieval method (semantic/keyword/literal), each feature match carries the prior page's own provenance, and the response includes top-level provenance breadcrumbs (how the evidence was gathered).",
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
                            "description": "Build a feature-vs-existing-code impact report for an indexed repo and ALWAYS write an interactive HTML (impact summary + evidence) into docs/features_memory. Returns a COMPACT summary — counts plus the existing files/symbols the feature touches, warnings, the HTML path, and PROVENANCE breadcrumbs (how the report was generated: hybrid retrieval with a per-method hit breakdown, manifests scanned, aggregation) — and keeps the full evidence in the HTML only (so it won't flood your context like a raw feature_context dump). Use to see how a proposed feature maps onto the codebase as it exists today.",
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
                            "description": "Write an interactive feature website into docs/features_memory with an embedded chaos-feature-manifest JSON block. PREFERRED: pass ONLY the structured `manifest` (feature, title, subtitle, claims>=3, modes>=2, nodes>=5 with file/lines/code, edges>=3, story>=3) and OMIT `html` — Chaos renders the full interactive page deterministically (same renderer as chaos add), so you never spend tokens authoring or transmitting raw HTML. Use after chaos_feature_context, not as a substitute for understanding the feature. Legacy: passing `html` still works but must include the interactive graph/story/architecture/code/evidence markers.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "repo": {"type": "string"},
                                    "slug": {"type": "string"},
                                    "title": {"type": "string"},
                                    "manifest": {"type": "object", "description": "FeatureManifest: {feature:{id,title,domain,summary}, title, subtitle, claims[], modes[], nodes[{id,label,subtitle,group,file,lines,role,code,evidence,confidence}], edges[{source,target,label,kind}], story[{id,title,body,node_ids}]}. Chaos renders the page from this."},
                                    "html": {"type": "string", "description": "LEGACY ONLY — omit to let Chaos render from the manifest (cheaper and consistent)."}
                                },
                                "required": ["repo", "slug", "title", "manifest"]
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
                            "description": "Write a CLIENT/USER-FACING interactive 'Feature guide' into docs/features_memory/<slug>-story.html: the feature explained as a code-free UI/UX user story (role-card personas, 'As a … I want … so that …' stories, a step-by-step scrollytelling walkthrough, outcomes), rendered by Chaos in the light editorial theme — you pass a structured manifest only, never HTML. Each walkthrough frame may carry a `preview` showing the REAL client UI (a captured screenshot, or a live iframe of a running route); Chaos cannot synthesise screens, so frames without a preview show an honest placeholder — ask for real captures. Optional extras: `brand_preset` (e.g. 'molecule') or `brand`/`hero_image`, persona `who`/`icon`/`includes`/`tier`, a permission `matrix`, a `callout`, and an end-of-page `game`. Confidence values are metadata, never shown to end users. Use for stakeholder/end-user presentations; use chaos_write_feature_website for the engineer-facing page. Compose from real understanding (chaos_feature_context / chaos_impact first); do not invent UI.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "repo": {"type": "string"},
                                    "slug": {"type": "string", "description": "Slug for the output filename docs/features_memory/<slug>-story.html."},
                                    "title": {"type": "string", "description": "Page title; used when the manifest omits one."},
                                    "manifest": {"type": "object", "description": "StoryboardManifest: {title, subtitle, audience, overall_confidence, personas[], stories[], frames[], outcomes[]} — NO code/file/line fields. Minimums: >=1 persona, >=2 stories, >=3 frames, >=1 outcome; confidences in [0,1]; story.frame_ids and persona references must resolve. A frame's optional `preview` is {kind:'image', src, alt, caption} for a captured screenshot (preferred) or {kind:'iframe', url, caption} for a running route; src/url must not be javascript:/vbscript:/data:text/html. Optional: `brand_preset` name, `hero_image` + `brand` {name,tagline,logo_src,href}, persona `who`/`icon`/`includes`/`tier`, `matrix` {columns, rows:[{capability, allowed[]}]}, `callout` {kicker,heading,intro,title,body,points[]}, `game` {kicker,heading,intro,instructions,rounds:[{prompt,context[],options:[{label,correct,explain}]}],win_message} (each round >=2 options, >=1 correct)."}
                                },
                                "required": ["repo", "slug", "title", "manifest"]
                            }
                        },
                        {
                            "name": "chaos_change_plan",
                            "description": "Decompose a proposed change into the FEATURES (L1 communities / god-nodes) it spans, with a dependency-aware check order — the top-down counterpart to flat retrieval. Matches the change description against community summary embeddings, ALSO seeding from a real git diff (`since`) AND from previously generated feature pages it correlates with (shared files → communities), so a curated existing feature deepens the decomposition. Each feature reports how it was surfaced via `+`-joined sources (semantic/diff/manifest) plus matched_by breadcrumbs, and the plan carries top-level provenance breadcrumbs. ALWAYS writes an interactive HTML plan to docs/features_memory/<slug>-plan.html and returns a COMPACT JSON summary (counts + per-feature label/confidence/via/check_order/top symbols + provenance + the HTML path), so it won't flood your context. Use it to answer 'how many features does this change involve, and in what order should I check them?'. Requires the repo to be indexed (chaos_analyze/chaos_add build the hierarchy).",
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
                        },
                        {
                            "name": "chaos_components",
                            "description": "Explain the CORE COMPONENTS of a big area — the orientation step BEFORE feature extraction. An area like 'OCL' is bigger than one feature: it spans several L1 communities. Given an `area` description (or none, for a repo-level overview) this zooms out one level and surfaces the communities that make up the area as COMPONENTS, each with its L3 summary, key symbols/files, languages, and a quotient-graph ROLE (entry/interface/core/foundation), plus how the components connect and a dependency-first READ ORDER so an agent understands the subsystem before drilling into any single feature. ALWAYS writes an interactive HTML overview to docs/features_memory/<slug>-components.html (with the manifest embedded under id=\"chaos-components-manifest\" so an agent can extract it) and returns a COMPACT JSON summary (counts, per-component label/role/read_order/top symbols/matched_by, relationships, related prior feature pages, PROVENANCE breadcrumbs, and the HTML path) so it won't flood your context. Also correlates the area with previously generated feature pages by shared files. Requires the repo to be indexed (chaos_analyze/chaos_add build the hierarchy).",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "repo": {"type": "string"},
                                    "area": {"type": "string", "description": "Area/subsystem to explain (e.g. 'OCL', 'access control layer'). Omit for a repo-level overview of the core components."},
                                    "output_html": {"type": "string", "description": "Override the default docs/features_memory/<slug>-components.html path."},
                                    "limit": {"type": "integer", "default": 8, "description": "Max components to surface."},
                                    "top_members": {"type": "integer", "default": 12, "description": "Representative members (symbols/files) loaded per component."}
                                },
                                "required": ["repo"]
                            }
                        },
                        {
                            "name": "chaos_features",
                            "description": "List ALL god-node FEATURES (L1 communities) that match a filter, grouped by where each sits in the user journey (entry → interface → core → foundation). This is the EXHAUSTIVE inventory counterpart to chaos_components: where chaos_components gives a curated, capped, ordered read-through of ONE area, chaos_features answers 'give me EVERY feature [in this layer / under this folder / about this topic]' with no curation and no cap. The single `filter` is AUTO-DETECTED — a path or a real directory name → FOLDER scope (features whose code lives under it); a single layer word like client/ui/api/core/contracts → that journey LAYER (so 'client features' means every entry-layer feature); anything else → a TOPIC match (summary-embedding cosine + label/summary keywords); omit it for the whole repo. Force the interpretation with `layer`/`folder`/`topic`. Only a topic filter needs the embedder; layer/folder/whole-repo listing is embedder-free. ALWAYS writes an interactive HTML inventory to docs/features_memory/<slug>-features.html (manifest embedded under id=\"chaos-features-manifest\") and returns a COMPACT JSON summary (resolved filter + how detected, total, per-layer counts, language counts, per-feature label/role/member_count/folders/top symbols/matched_by, PROVENANCE breadcrumbs, the HTML path). Requires the repo to be indexed (chaos_analyze/chaos_add build the hierarchy).",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "repo": {"type": "string", "description": "Repository to list. Omit when passing `project`."},
                                    "project": {"type": "string", "description": "List features across ALL repos of this PROJECT instead of one repo: every member repo's features in one journey-layered inventory, each card tagged with its repo alias (client/backend/contracts/…) and annotated with the project's cross-repo links (→ backend:auth-api (http_route)). The HTML goes to the project workspace (~/.chaos/projects/<slug>/ or $CHAOS_PROJECT_DIR)."},
                                    "filter": {"type": "string", "description": "Auto-detected filter: a path/dir → folder; a layer word (client/ui/api/core/contracts) → layer; else a topic. Omit for the whole repo/project."},
                                    "layer": {"type": "string", "description": "Force a layer filter: entry|interface|core|foundation (or a synonym like client/api/contracts)."},
                                    "folder": {"type": "string", "description": "Force a folder filter: features with code under this path."},
                                    "topic": {"type": "string", "description": "Force a topic (semantic + keyword) filter."},
                                    "output_html": {"type": "string", "description": "Override the default docs/features_memory/<slug>-features.html path."},
                                    "limit": {"type": "integer", "default": 0, "description": "Cap features surfaced; 0 = all (default — exhaustive)."}
                                },
                                "required": []
                            }
                        },
                        {
                            "name": "chaos_project",
                            "description": "Manage CROSS-REPOSITORY projects: a named set of indexed repos (client, backend, contracts, infra, …). Chaos detects feature→feature CROSS-REPO LINKS between members from the persisted index (consumer → provider): `package_dep` (imports a package another member publishes), `abi` (references a Solidity contract defined elsewhere), `http_route` (a fetch/axios call path matches a registered route). Links attach at the feature (L1) level with evidence + provenance, and refresh AUTOMATICALLY after chaos_analyze/chaos_add on any member (hash-gated — a no-change re-index relinks nothing). Actions: create (idempotent), add_repo (attach an INDEXED repo under an alias; links immediately), list, status (members, staleness, links by kind, embedder consistency), relink (`force` overrides the gate). Use chaos_features with `project` for the cross-repo feature inventory.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "action": {"type": "string", "enum": ["create", "add_repo", "list", "status", "relink"], "description": "What to do."},
                                    "project": {"type": "string", "description": "Project name (required for every action except list)."},
                                    "repo": {"type": "string", "description": "Repository path or name to attach (add_repo)."},
                                    "alias": {"type": "string", "description": "Project-scoped alias for the repo (client/backend/contracts/infra/…). Defaults to the repo name."},
                                    "force": {"type": "boolean", "default": false, "description": "relink: re-detect even when no member's root hash moved."}
                                },
                                "required": ["action"]
                            }
                        },
                        {
                            "name": "chaos_clean",
                            "description": "DESTRUCTIVE: wipe the persisted index — one repository (pass `repo`) or EVERYTHING (omit it). Pass `artifacts: true` to also delete the generated files on disk (the repo's chaos-obsidian-vault/ and docs/features_memory/, plus all project workspaces when wiping everything) for a truly clean slate before re-validation. Requires `confirm: true` — refuse to guess; only call this when the user explicitly asked to clean/reset. Reports exactly what was removed. The schema survives (no re-migrate needed). Cleaning does NOT imply re-indexing: stop after the wipe unless the user also asked to rebuild — the index simply stays empty until a chaos_analyze is requested.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "repo": {"type": "string", "description": "Repository path or name to clear. OMIT to wipe every repository."},
                                    "artifacts": {"type": "boolean", "default": false, "description": "Also delete generated files on disk (vault, feature pages, project workspaces)."},
                                    "confirm": {"type": "boolean", "description": "Must be true. Guard against accidental wipes — set it only on explicit user intent."}
                                },
                                "required": ["confirm"]
                            }
                        },
                        {
                            "name": "chaos_graph",
                            "description": "Export an already-indexed repository as a standalone interactive HTML graph (the full L0 node/edge view) read from the persisted index — embedder-free, writes one self-contained file. Defaults to docs/features_memory/graph.html inside the repo (so chaos_clean --artifacts sweeps it); override with `output`. For the feature-level map (L1 communities + quotient graph) use chaos_obsidian / chaos_refresh instead, which write feature-map.html.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "repo": {"type": "string"},
                                    "output": {"type": "string", "description": "Output HTML path. Defaults to <repo>/docs/features_memory/graph.html."}
                                },
                                "required": ["repo"]
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
        "chaos_stack" => {
            let repo = args
                .get("repo")
                .and_then(Value::as_str)
                .context("repo is required")?;
            let opts = crate::stack::StackOptions {
                output_html: args
                    .get("output_html")
                    .and_then(Value::as_str)
                    .map(PathBuf::from),
            };
            let summary = crate::stack::run(storage, repo, &opts).await?;
            Ok(tool_text(serde_json::to_string_pretty(&summary)?))
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
                let mut answer = query_repo(storage, repo.id, embedder, question, limit).await?;
                // Return-only surface: excerpt chunk contents (the full text
                // stays in the index; the agent can open the file).
                crate::query::cap_hits_for_return(&mut answer.hits);
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
            let provenance = feature_context_provenance(&postgres, &features_dir, &feature_matches);
            let mut response = FeatureContextResponse {
                task: task.to_string(),
                postgres,
                features_dir,
                warnings,
                feature_matches,
                provenance,
            };
            let output_html = args.get("output_html").and_then(Value::as_str);
            if let Some(output_html) = output_html {
                // The HTML keeps the FULL evidence; the return gets excerpts.
                write_feature_context_html(Path::new(output_html), &response)?;
            }
            crate::feature_context::cap_response_for_return(&mut response);
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
            let html = args.get("html").and_then(Value::as_str);
            let manifest = args.get("manifest").context("manifest is required")?;
            let repo = storage
                .find_repository(repo)
                .await?
                .context("repository is not indexed")?;
            // Preferred path: NO html argument — Chaos renders the interactive
            // page from the manifest (same deterministic renderer `chaos add`
            // uses), so the LLM never spends tokens authoring or transmitting
            // raw HTML. The legacy html path remains for back-compat.
            let (path, rendered_by) = match html {
                None => (
                    write_manifest_feature_website(&repo.root_path, slug, title, manifest)?,
                    "chaos (manifest-driven)",
                ),
                Some(html) => (
                    write_llm_feature_website(&repo.root_path, slug, title, html, manifest)?,
                    "llm-html (legacy)",
                ),
            };
            Ok(tool_text(serde_json::to_string_pretty(&json!({
                "output_html": path,
                "manifest_embedded": true,
                "rendered_by": rendered_by
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
            let hierarchy = storage.load_community_hierarchy(&repo, 14).await?;
            let hier = crate::hierarchy_export::write_hierarchy(&output, &output, &hierarchy)?;
            Ok(tool_text(serde_json::to_string_pretty(&json!({
                "output": summary.output,
                "repo_id": repo.id,
                "topics": summary.topics,
                "node_notes": summary.node_notes,
                "edges": summary.edges,
                "community_notes": hier.community_notes,
                "feature_map_html": hier.feature_map_html
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
            let hierarchy = storage.load_community_hierarchy(&repo, 14).await?;
            let summary = refresh_project_exports(
                &graph,
                &obsidian_output,
                &features_dir,
                all_features,
                &repo_root,
                Some(&hierarchy),
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
                "skipped_feature_pages": summary.skipped_feature_pages,
                "community_notes": summary.community_notes,
                "feature_map_html": summary.feature_map_html
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
        "chaos_components" => {
            let repo = args
                .get("repo")
                .and_then(Value::as_str)
                .context("repo is required")?;
            let area = args.get("area").and_then(Value::as_str);
            let opts = crate::components::ComponentsOptions {
                output_html: args
                    .get("output_html")
                    .and_then(Value::as_str)
                    .map(PathBuf::from),
                limit: args.get("limit").and_then(Value::as_u64).unwrap_or(8) as usize,
                top_members: args
                    .get("top_members")
                    .and_then(Value::as_u64)
                    .unwrap_or(12) as usize,
            };
            let summary = crate::components::run(storage, embedder, repo, area, &opts).await?;
            Ok(tool_text(serde_json::to_string_pretty(&summary)?))
        }
        "chaos_features" => {
            let repo = args.get("repo").and_then(Value::as_str);
            let project = args.get("project").and_then(Value::as_str);
            let filter = args.get("filter").and_then(Value::as_str);
            let opts = crate::feature_inventory::FeatureInventoryOptions {
                output_html: args
                    .get("output_html")
                    .and_then(Value::as_str)
                    .map(PathBuf::from),
                limit: args.get("limit").and_then(Value::as_u64).unwrap_or(0) as usize,
                layer: args.get("layer").and_then(Value::as_str).map(String::from),
                folder: args.get("folder").and_then(Value::as_str).map(String::from),
                topic: args.get("topic").and_then(Value::as_str).map(String::from),
            };
            let summary = match (project, repo) {
                (Some(project), _) => {
                    crate::feature_inventory::run_project(
                        storage,
                        Some(embedder),
                        project,
                        filter,
                        &opts,
                    )
                    .await?
                }
                (None, Some(repo)) => {
                    crate::feature_inventory::run(storage, Some(embedder), repo, filter, &opts)
                        .await?
                }
                (None, None) => anyhow::bail!("pass `repo` or `project`"),
            };
            Ok(tool_text(serde_json::to_string_pretty(&summary)?))
        }
        "chaos_project" => {
            let action = args
                .get("action")
                .and_then(Value::as_str)
                .context("action is required: create | add_repo | list | status | relink")?;
            let name = || {
                args.get("project")
                    .and_then(Value::as_str)
                    .context("project is required")
            };
            let summary = match action {
                "create" => crate::project::create(storage, name()?).await?,
                "add_repo" => {
                    let repo = args
                        .get("repo")
                        .and_then(Value::as_str)
                        .context("repo is required")?;
                    let alias = args.get("alias").and_then(Value::as_str);
                    crate::project::add_repo(storage, name()?, repo, alias).await?
                }
                "list" => crate::project::list(storage).await?,
                "status" => crate::project::status(storage, name()?).await?,
                "relink" => {
                    let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
                    crate::project::relink(storage, name()?, force).await?
                }
                other => anyhow::bail!(
                    "unknown action `{other}` — use create | add_repo | list | status | relink"
                ),
            };
            Ok(tool_text(serde_json::to_string_pretty(&summary)?))
        }
        "chaos_help" => Ok(tool_text(AGENT_GUIDE.to_string())),
        "chaos_clean" => {
            if !args
                .get("confirm")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                anyhow::bail!(
                    "chaos_clean is destructive — pass confirm: true (and only when the user explicitly asked to clean/reset)"
                );
            }
            let repo = args.get("repo").and_then(Value::as_str);
            let artifacts = args
                .get("artifacts")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let summary = crate::run_clean(storage, repo, artifacts).await?;
            Ok(tool_text(serde_json::to_string_pretty(&summary)?))
        }
        "chaos_graph" => {
            let repo = args
                .get("repo")
                .and_then(Value::as_str)
                .context("repo is required")?;
            let repo = storage
                .find_repository(repo)
                .await?
                .context("repository is not indexed")?;
            let output = args
                .get("output")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    Path::new(&repo.root_path).join("docs/features_memory/graph.html")
                });
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent)?;
            }
            let graph = storage.load_graph_export(&repo).await?;
            crate::graph_export::write_graph_html(&output, &graph)?;
            Ok(tool_text(serde_json::to_string_pretty(&json!({
                "output": output,
                "repo_id": repo.id,
                "nodes": graph.nodes.len(),
                "edges": graph.edges.len()
            }))?))
        }
        _ => anyhow::bail!("unknown tool: {name}"),
    }
}

/// The `chaos_help` payload: cross-tool workflow guidance MCP-only sessions
/// otherwise never see (the plugin's SKILL.md carries it for plugin users).
/// Static text — zero DB/embedder work, and zero tokens until requested.
const AGENT_GUIDE: &str = "\
Chaos Substrate — persistent code knowledge memory (Postgres + pgvector). Tool order and workflows:

WORKFLOWS
  first index        chaos_analyze {repo_path}  — full graph + embeddings + feature hierarchy
  after editing      chaos_add {repo_path, message}  — index only the git-changed files, refresh artifacts, write a feature/bug page
  sanity-check       chaos_stats {repo}  — what the index holds (read-only, embedder-free)
  what's the stack   chaos_stack {repo}  — declared dependencies, scripts, CDK stacks/resources, configs, languages — LISTED, with explicit coverage notes (embedder-free)
  ask a question     chaos_query {repo, question, hierarchical: true}  — feature-routed retrieval; flat search without the flag
  grasp a big area   chaos_components {repo, area?}  — curated component overview with a read order (run BEFORE feature work)
  list features      chaos_features {repo | project, filter?}  — exhaustive inventory; filter auto-detects folder | layer (client/api/core/contracts) | topic
  scope a change     chaos_change_plan {repo, change_description, since?}  — which features a change spans, in check order
  gather evidence    chaos_feature_context {repo, task}  — implementation context; treat its warnings as blockers
  impact (before)    chaos_impact {repo, feature}  — how a proposed feature maps onto today's code, compact return + HTML
  document (eng)     chaos_write_feature_website {repo, slug, title, manifest}  — OMIT html: Chaos renders the page from the manifest
  document (users)   chaos_write_storyboard {repo, slug, title, manifest}  — code-free feature guide for stakeholders
  cross-repo         chaos_project {action: create | add_repo | list | status | relink}  — link client/backend/contracts/infra repos; then chaos_features {project}
  exports            chaos_obsidian / chaos_refresh  — regenerate vault + pages from the index, no embedder
  graph view         chaos_graph {repo, output?}  — standalone interactive L0 node/edge HTML (feature map comes from obsidian/refresh)
  fresh start        chaos_clean {repo?, artifacts?, confirm: true}  — DESTRUCTIVE index wipe (one repo or all); artifacts also deletes generated files

RULES OF THUMB
  - Index before anything else; chaos_add after each change keeps memory fresh (hash-gated: unchanged content costs zero embedder calls).
  - Returns are compact excerpts (chunk text capped, lists capped); the generated HTML under docs/features_memory/ keeps FULL evidence.
  - Compose feature pages from chaos_feature_context evidence, never from chaos_query alone; pass manifests, never raw HTML.
  - Cross-repo: all member repos must share one embedder config; links refresh automatically after analyze/add.
  - CLI equivalent exists for everything (`chaos help` in a shell); full ops reference: RUNBOOK.md, canonical tool table: README.md.
";

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

/// Render the feature page from the manifest alone — the deterministic Rust
/// renderer `chaos add` already uses. The minimum-evidence contract still
/// applies; only the HTML-authoring burden moves off the LLM.
fn write_manifest_feature_website(
    repo_root: &str,
    slug: &str,
    title: &str,
    manifest: &Value,
) -> Result<PathBuf> {
    validate_manifest_minimums(manifest)?;
    // Tolerate a manifest that leaves title/subtitle to the tool arguments.
    let mut value = manifest.clone();
    if let Value::Object(map) = &mut value {
        if map
            .get("title")
            .and_then(Value::as_str)
            .is_none_or(|t| t.trim().is_empty())
        {
            map.insert("title".into(), json!(title));
        }
        map.entry("subtitle").or_insert_with(|| json!(""));
    }
    let parsed: crate::feature_context::FeatureManifest = serde_json::from_value(value).context(
        "manifest must match the FeatureManifest schema (feature, title, subtitle, claims, modes, nodes, edges, story)",
    )?;
    let slug = safe_slug(slug);
    let output = Path::new(repo_root)
        .join("docs/features_memory")
        .join(format!("{slug}-explanation.html"));
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        &output,
        crate::feature_export::render_feature_website(&parsed)?,
    )?;
    Ok(output)
}

/// Minimum-evidence contract shared by both rendering paths.
pub(crate) fn validate_manifest_minimums(manifest: &Value) -> Result<()> {
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
    Ok(())
}

pub(crate) fn validate_feature_website_contract(html: &str, manifest: &Value) -> Result<()> {
    validate_manifest_minimums(manifest)?;

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
        // Embeddings for unchanged content survive the wipe (restored by content
        // hash inside the replace transaction).
        let reused = storage.replace_repo_index(repo.id, &result).await?;
        let missing = storage
            .chunks_missing_embeddings(
                repo.id,
                embedder.provider(),
                embedder.model_id(),
                embedder.dimensions(),
            )
            .await?;
        crate::embedding::embed_missing_chunks(storage, embedder, &missing).await?;
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
        // P6: relink every project containing this repo (hash-gated).
        let projects = crate::project::relink_projects_for_repo(storage, repo.id).await;
        let feature_communities = detection.communities.iter().filter(|c| c.size >= 2).count();
        Result::<_, anyhow::Error>::Ok(json!({
            "projects": projects,
            "repo_id": repo.id,
            "files": result.files.len(),
            "nodes": result.nodes.len(),
            "edges": result.edges.len(),
            "chunks": result.chunks.len(),
            "embedded_chunks": missing.len(),
            "reused_embeddings": reused,
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
    fn agent_guide_names_every_other_tool() {
        // Sync guard: if a tool is added without teaching the guide about it,
        // this fails. (chaos_help itself is the one returning the guide.)
        for tool in [
            "chaos_analyze",
            "chaos_add",
            "chaos_stats",
            "chaos_stack",
            "chaos_query",
            "chaos_feature_context",
            "chaos_impact",
            "chaos_write_feature_website",
            "chaos_obsidian",
            "chaos_refresh",
            "chaos_write_storyboard",
            "chaos_change_plan",
            "chaos_components",
            "chaos_features",
            "chaos_project",
            "chaos_clean",
            "chaos_graph",
        ] {
            assert!(AGENT_GUIDE.contains(tool), "guide missing {tool}");
        }
    }

    #[test]
    fn manifest_driven_website_renders_without_llm_html() {
        let dir = tempfile::tempdir().unwrap();
        let node = |id: &str| {
            json!({
                "id": id, "label": id, "subtitle": "s", "group": "core",
                "file": "src/lib.rs", "lines": "1-10", "role": "core",
                "code": "fn x() {}"
            })
        };
        let manifest = json!({
            "feature": {"id": "f1", "title": "Auth", "domain": "core", "summary": "sums"},
            "title": "Auth feature",
            "subtitle": "How auth works",
            "claims": [
                {"id": "c1", "title": "t", "body": "b", "confidence": 0.9, "node_ids": ["n1"]},
                {"id": "c2", "title": "t", "body": "b", "confidence": 0.9, "node_ids": ["n2"]},
                {"id": "c3", "title": "t", "body": "b", "confidence": 0.9, "node_ids": ["n3"]}
            ],
            "modes": [
                {"id": "m1", "title": "happy", "node_ids": ["n1"]},
                {"id": "m2", "title": "error", "node_ids": ["n2"]}
            ],
            "nodes": [node("n1"), node("n2"), node("n3"), node("n4"), node("n5")],
            "edges": [
                {"source": "n1", "target": "n2", "label": "calls"},
                {"source": "n2", "target": "n3", "label": "calls"},
                {"source": "n3", "target": "n4", "label": "calls"}
            ],
            "story": [
                {"id": "s1", "title": "step 1"},
                {"id": "s2", "title": "step 2"},
                {"id": "s3", "title": "step 3"}
            ]
        });
        let path = write_manifest_feature_website(
            dir.path().to_str().unwrap(),
            "auth-feature",
            "Auth feature",
            &manifest,
        )
        .expect("manifest-driven render should succeed");
        let html = std::fs::read_to_string(&path).unwrap();
        assert!(html.contains("chaos-feature-manifest"), "manifest embedded");
        assert!(html.contains("Auth feature"));

        // Thin manifests are still rejected (the evidence contract holds).
        let thin = json!({"claims": [], "modes": [], "nodes": [], "edges": [], "story": []});
        assert!(write_manifest_feature_website(
            dir.path().to_str().unwrap(),
            "thin",
            "Thin",
            &thin
        )
        .is_err());
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
