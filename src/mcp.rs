use crate::{
    embedding::{build_embedder, Embedder},
    export_util::{escape_script_json, features_memory_dir, html_escape_full},
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
                    "instructions": "Persistent code knowledge memory. Index with chaos_analyze (full) or chaos_add (after edits); ask with chaos_query (hierarchical=true for feature routing); orient with chaos_components / chaos_features; tech stack & infra via chaos_stack; scope changes with chaos_change_plan; cross-repo via chaos_project. Tool returns are compact excerpts — full evidence lives in the generated HTML pages. A feature deep-dive ends with chaos_write_feature_website (persist your explanation as a page, manifest only) — not chat-only. Call chaos_help for workflows and tool order."
                }
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"tools": tool_definitions()}
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

/// Single source of truth for the MCP tool roster, in `tools/list` order.
/// The `tools/list` builder and the parity/anti-drift tests all read this;
/// adding a tool means adding it here, in [`tool_definitions`], and in the
/// dispatch match — the tests fail on any partial wiring.
pub(crate) const TOOL_NAMES: [&str; 24] = [
    "chaos_help",
    "chaos_analyze",
    "chaos_add",
    "chaos_stats",
    "chaos_stack",
    "chaos_pages",
    "chaos_query",
    "chaos_feature_context",
    "chaos_impact",
    "chaos_write_feature_website",
    "chaos_obsidian",
    "chaos_refresh",
    "chaos_write_storyboard",
    "chaos_sui_migration_impact",
    "chaos_change_plan",
    "chaos_components",
    "chaos_features",
    "chaos_compose",
    "chaos_project",
    "chaos_feature_story",
    "chaos_clean",
    "chaos_gaps",
    "chaos_graph",
    "chaos_usage",
];

/// The `tools/list` tool schemas — one small `json!` literal per tool
/// (one giant literal used to need `#![recursion_limit = "256"]`).
/// Roster order matches [`TOOL_NAMES`]; a test (and a debug assert) enforce it.
fn tool_definitions() -> Vec<Value> {
    let tools = tool_definition_literals();
    debug_assert_eq!(
        tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        TOOL_NAMES.to_vec(),
        "tools/list roster diverged from TOOL_NAMES"
    );
    tools
}

fn tool_definition_literals() -> Vec<Value> {
    vec![
        json!({
            "name": "chaos_help",
            "description": "The agent guide for this server: recommended tool ORDER and WORKFLOWS (first index, incremental updates, asking questions, orienting in a big codebase, scoping a change, documenting a feature, cross-repo projects, starting clean), plus token notes (returns are excerpts; HTML pages keep full evidence). Costs nothing until called — use it once when you first meet this server, or whenever unsure which tool fits.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        }),
        json!({
            "name": "chaos_analyze",
            "description": "Analyze and persist a repository knowledge graph and real embeddings. Replaces stale indexed data for that repository.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo_path": {"type": "string"}
                },
                "required": ["repo_path"]
            }
        }),
        json!({
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
        }),
        json!({
            "name": "chaos_stats",
            "description": "Report index statistics for an already-indexed repository, read from Postgres: totals (files, nodes, edges, chunks, embedded vs missing, split chunks) plus breakdowns of nodes by kind, edges by kind, chunks by type, and files by language. Read-only and embedder-free — use to explain or sanity-check what an analyze/add produced.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string"}
                },
                "required": ["repo"]
            }
        }),
        json!({
            "name": "chaos_stack",
            "description": "Report the TECH STACK of an already-indexed repository, LISTED rather than counted (chaos_stats only counts these node kinds): manifest-DECLARED dependencies by ecosystem (npm/cargo — name, versions, runtime-vs-dev scope, and how many workspace manifests declare each, widest-declared first), npm scripts, deployment resources (AWS CDK app entrypoints, Stack classes, and L2 constructs grouped by cloud service), indexed JS/TS configs, the repo's exposed API SURFACE from persisted user-surface nodes (HTTP routes with method + path, GraphQL root fields grouped Query./Mutation./Subscription. — SDL-derived only — and CLI commands), and the file-language breakdown. Read-only and embedder-free. ALWAYS writes an interactive HTML inventory to docs/features_memory/stack.html (manifest embedded under id=\"chaos-stack-manifest\") and returns a COMPACT JSON summary (capped lists with *_omitted counts; every entry lives in the HTML). The return states its COVERAGE explicitly — what the index extracts vs what it does not yet (Dockerfiles, CI workflows, pyproject.toml, foundry.toml, Terraform, code-first GraphQL schemas), so read those files directly if they matter. Use it to answer \"what is this repo built with / what infrastructure does it use / what API does it expose?\" without grepping manifests.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string"},
                    "output_html": {"type": "string", "description": "Override the default docs/features_memory/stack.html path."}
                },
                "required": ["repo"]
            }
        }),
        json!({
            "name": "chaos_pages",
            "description": "List the GENERATED feature-memory pages of a repository — what chaos has ALREADY extracted. Scans docs/features_memory (or features_dir) and returns every HTML page with its KIND (feature / story / components / features / composed / stack / impact / change-plan / feature-map; unrecognised files are listed as `other`, never hidden), the tool that writes that kind, its title, and its modified time, newest first, plus by-kind counts. Read-only, embedder-free, pure filesystem. Use this INSTEAD of `ls`/globbing to check whether a feature was already extracted before running a new deep-dive, and to find the page to reopen.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "Indexed repo name/path, or any directory containing generated pages."},
                    "features_dir": {"type": "string", "description": "Scan this directory instead of <repo>/docs/features_memory (e.g. a project workspace)."}
                },
                "required": ["repo"]
            }
        }),
        json!({
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
        }),
        json!({
            "name": "chaos_feature_context",
            "description": "Build focused implementation context for a feature or task. Reads Postgres retrieval plus generated feature-memory manifests and returns warnings when expected paths/docs are missing; treat warnings as blockers before writing. Mirroring chaos_impact, it ALWAYS writes an interactive HTML to docs/features_memory/<slug>-context.html (override with output_html) and returns a COMPACT payload: counts (hits + per-channel + distinct-after-dedup), deduped, ranked evidence lines (symbol, file, lines, kind, relevance_pct, retrieved_by) with the TOP hits' verbatim bodies inlined as `code_excerpt` (bounded), relevance-floored related_pages (title/domain/score/shared symbols, not their full claims/code), warnings, and provenance breadcrumbs. The FULL verbatim evidence (every hit's content + correlated node code) lives in the written HTML, embedded as an agent-extractable JSON block under id=\"chaos-feature-context-data\". Read the actual body — inlined code_excerpt, the HTML data block, or the source — before making any behavioral claim; do not infer behavior from symbol names alone. Re-running retrieval is unnecessary, but reading source to confirm a claim is fine. Each hit is tagged with its retrieval method (semantic/keyword/literal). Finish the drill-down by persisting your composed explanation with chaos_write_feature_website (manifest only), so the deep-dive survives the conversation.",
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
        }),
        json!({
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
        }),
        json!({
            "name": "chaos_write_feature_website",
            "description": "Write an interactive feature website into docs/features_memory with an embedded chaos-feature-manifest JSON block. PREFERRED: pass ONLY the structured `manifest` (purpose [REQUIRED], feature, title, subtitle, examples, claims>=3, modes>=2, nodes>=5 with file/lines/code, edges>=3, story>=3) and OMIT `html` — Chaos renders the full interactive page deterministically (same renderer as chaos add), so you never spend tokens authoring or transmitting raw HTML. The page opens with the `purpose` band (plain language: what the feature was made for, who uses it) and a 'How you'd use it' section from `examples` — include at least one simple example whenever the feature has a callable surface; each example's node_ids highlight the code path it exercises. Use after chaos_feature_context, not as a substitute for understanding the feature. Legacy: passing `html` still works but must include the interactive graph/story/architecture/code/evidence markers.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string"},
                    "slug": {"type": "string"},
                    "title": {"type": "string"},
                    "manifest": {"type": "object", "description": "FeatureManifest: {purpose (REQUIRED plain-language 'what this was made for'), feature:{id,title,domain,summary}, title, subtitle, examples[{title,description,steps[],code,language,node_ids}] (>=1 simple usage example recommended), claims[], modes[], nodes[{id,label,subtitle,group,file,lines,role,code,evidence,confidence}], edges[{source,target,label,kind}], story[{id,title,body,node_ids}]}. Chaos renders the page from this."},
                    "html": {"type": "string", "description": "LEGACY ONLY — omit to let Chaos render from the manifest (cheaper and consistent)."}
                },
                "required": ["repo", "slug", "title", "manifest"]
            }
        }),
        json!({
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
        }),
        json!({
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
        }),
        json!({
            "name": "chaos_write_storyboard",
            "description": "Write a CLIENT/USER-FACING interactive 'Feature guide' into docs/features_memory/<slug>-story.html: the feature explained as a code-free UI/UX user story (role-card personas, 'As a … I want … so that …' stories, a step-by-step scrollytelling walkthrough, outcomes), rendered by Chaos in the light editorial theme — you pass a structured manifest only, never HTML. Each walkthrough frame may carry a `preview` showing the REAL client UI (a captured screenshot, or a live iframe of a running route); Chaos cannot synthesise screens, so frames without a preview render text-only (no mockup, no placeholder); add real captures when you have them. Optional extras: `brand_preset` (e.g. 'molecule') or `brand`/`hero_image`, persona `who`/`icon`/`includes`/`tier`, a permission `matrix`, a `callout`, and an end-of-page `game`. Confidence values are metadata, never shown to end users. Use for stakeholder/end-user presentations; use chaos_write_feature_website for the engineer-facing page. Compose from real understanding (chaos_feature_context / chaos_impact first); do not invent UI.",
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
        }),
        json!({
            "name": "chaos_sui_migration_impact",
            "description": "Build a SUI MIGRATION IMPACT report for an indexed Ethereum/Solana/mixed Web3 repository — answers 'if this project moves to Sui, which existing features are affected, which Sui primitives map to them, and what should be reviewed first?'. Detects the source stack from the persisted index (Solidity/Hardhat/OpenZeppelin/ERC standards, Anchor/SPL/Metaplex/PDAs, ethers/viem/wagmi/@solana clients, IPFS/Arweave/S3 storage, encryption/token-gating) plus disk probes for toolchain configs; maps evidence files onto the L1 feature communities; triggers source-pattern → Sui concept mappings (objects/dynamic fields, Coin/Kiosk/Display, capabilities, PTBs, package upgrades, events+GraphQL) each citing the compiled-in 'sui-official' docs profile (official Sui / Walrus / Seal pages with URL + verified date); classifies storage flows (Walrus blob / Walrus+Seal / keep-offchain / review) and gives explicit Seal verdicts including 'not-needed'; correlates prior generated feature pages; proposes a review order. Read-only and embedder-free. ALWAYS writes docs/features_memory/sui-migration-impact.html (manifest embedded under id=\"chaos-sui-migration-manifest\") and returns a COMPACT JSON summary (capped lists with *_omitted counts) with provenance breadcrumbs. Evidence-triggered only: no signal, no claim — and it maps impact, it does NOT generate Move code or promise automatic migration.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string"},
                    "source": {"type": "string", "enum": ["auto", "ethereum", "solana", "mixed"], "default": "auto", "description": "Source stack; auto detects from evidence."},
                    "output_html": {"type": "string", "description": "Override the default docs/features_memory/sui-migration-impact.html path."},
                    "features_dir": {"type": "string", "description": "Feature-page directory for prior-page correlation."},
                    "limit": {"type": "integer", "default": 12, "description": "Max affected features in the compact return."}
                },
                "required": ["repo"]
            }
        }),
        json!({
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
        }),
        json!({
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
        }),
        json!({
            "name": "chaos_features",
            "description": "List ALL god-node FEATURES (L1 communities) that match a filter, grouped by where each sits in the user journey (entry → interface → core → foundation). This is the EXHAUSTIVE inventory counterpart to chaos_components: where chaos_components gives a curated, capped, ordered read-through of ONE area, chaos_features answers 'give me EVERY feature [in this layer / under this folder / about this topic]' with no curation and no cap. The single `filter` is AUTO-DETECTED — a path or a real directory name → FOLDER scope (features whose code lives under it); a single layer word like client/ui/api/core/contracts → that journey LAYER (so 'client features' means every entry-layer feature); anything else is first tried as a layer BY MEANING (embedding cosine against per-layer prototype phrasings — 'backend', 'client app', 'devops', 'API endpoints' resolve semantically, no keyword list; 'backend' spans interface+core) and only then falls to a TOPIC match (summary-embedding cosine + label/summary keywords); omit it for the whole repo. Force the interpretation with `layer`/`folder`/`topic`. Exact layer words, folders and whole-repo listing are embedder-free; semantic layer routing and topic matching use the embedder. ALWAYS writes an interactive HTML inventory to docs/features_memory/<slug>-features.html (manifest embedded under id=\"chaos-features-manifest\") and returns a COMPACT JSON summary sized to stay inline in agent context: resolved filter + how detected, total, per-layer counts, language counts, domain group names, ONE READABLE LINE PER FEATURE (label — layer role, member count · extra folders · short symbols; topic matches append why-it-matched), PROVENANCE breadcrumbs, the HTML path. Full per-feature detail (full symbol paths, files, breadcrumbs) lives in the HTML manifest. The HTML groups features into HUMAN-READABLE DOMAINS — folder-derived automatically; when you compose a curated grouping with notes for your answer, CALL AGAIN with the same repo/filter plus `curation` so the page carries your domains and one-line notes too (cheap re-render, tiny receipt return). Requires the repo to be indexed (chaos_analyze/chaos_add build the hierarchy).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "Repository to list. Omit when passing `project`."},
                    "project": {"type": "string", "description": "List features across ALL repos of this PROJECT instead of one repo: every member repo's features in one journey-layered inventory, each card tagged with its repo alias (client/backend/contracts/…) and annotated with the project's cross-repo links (→ backend:auth-api (http_route)). The HTML goes to the project workspace (~/.chaos/projects/<slug>/ or $CHAOS_PROJECT_DIR)."},
                    "filter": {"type": "string", "description": "Auto-detected filter: a path/dir → folder; a layer word (client/ui/api/core/contracts) → layer; any other phrase is tried as a layer by meaning ('backend', 'client app') before falling to a topic. Omit for the whole repo/project."},
                    "layer": {"type": "string", "description": "Force a layer filter: entry|interface|core|foundation (or a synonym like client/api/contracts)."},
                    "folder": {"type": "string", "description": "Force a folder filter: features with code under this path."},
                    "topic": {"type": "string", "description": "Force a topic (semantic + keyword) filter."},
                    "output_html": {"type": "string", "description": "Override the default docs/features_memory/<slug>-features.html path."},
                    "limit": {"type": "integer", "default": 0, "description": "Cap features surfaced; 0 = all (default — exhaustive)."},
                    "curation": {
                        "type": "object",
                        "description": "OPTIONAL second pass that makes the generated HTML human-first: after reading the inventory, call again with the SAME repo/filter plus this curation — the domain groupings and one-line notes you composed for your answer anyway. Shape: {groups: [{title, icon?, blurb?, features: [{label, note?}]}]} — title is the human heading ('IP-NFT Minting flow — the wizard'), icon an optional emoji, blurb an optional paragraph, label a feature label from the inventory (full or unique trailing fragment), note the one-line 'what's in it'. Chaos re-runs the identical selection and renders YOUR domains as the page's primary sections; unplaced features fall back to folder-derived domains (tagged auto). The return is a tiny render receipt, not the inventory again."
                    }
                },
                "required": []
            }
        }),
        json!({
            "name": "chaos_compose",
            "description": "THE page-generation surface: whenever the user asks for a webpage, website, or interactive info page over chaos knowledge, use THIS tool — do not stitch together the side-pages of chaos_features/chaos_stack/chaos_components (those remain data/inventory tools). Composes ONE page (or a SITE, with feature_pages=true) from knowledge-base-backed SECTIONS instead of generating several similar standalone pages. Pick the sections ('features' — the feature inventory with each feature's concise L3 explanation; 'correlations' — files shared between those features plus prior generated pages that overlap them; 'stack' — declared dependencies/scripts/deployment resources), an AUDIENCE (free-text `persona` like 'a very beginner software engineer who has no idea about the stack', resolved to beginner|practitioner|expert BY MEANING via prototype embeddings — or an explicit `level`, embedder-free), and a STYLE preset ('editorial' light default | 'blade-runner' dark neon; plus `brand_preset` e.g. 'molecule'). Chaos resolves every section from the PERSISTED INDEX and prior generated manifests ONLY — it never parses source files, and a section it cannot serve (repo not indexed, no L1 hierarchy, unknown section/style) is a LOUD ERROR naming what is missing and the command that fixes it. If this tool fails, REPORT the failure to the user as-is; do NOT fall back to rg/grep/scripts to fake the page. Writes docs/features_memory/<slug>-composed.html with an embedded `chaos-composed-manifest` carrying every section's full data for agent consumption, and returns a COMPACT JSON summary. The composition is CONTENT-HASHED (`content_hash` in manifest and return): recomposing the same request over unchanged knowledge returns `cached: true` without writing — treat the hash as your dedup key and do not re-ingest a composition you already hold.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string"},
                    "sections": {"type": "array", "items": {"type": "string"}, "description": "Sections in render order: features | correlations | stack (aliases: explanations→features, tech-stack→stack). Unknown names are an error listing the vocabulary."},
                    "persona": {"type": "string", "description": "Free-text audience description; routed to a detail level by embedding cosine against prototype phrasings (no keyword list). Needs the embedder — otherwise pass `level`."},
                    "level": {"type": "string", "enum": ["beginner", "practitioner", "expert"], "description": "Explicit detail level (embedder-free). beginner = plain language, jargon collapsed, read-order hints; expert = symbols/files expanded."},
                    "style": {"type": "string", "description": "Style preset: editorial (default light) | blade-runner (dark neon token override). Unknown style = error, no improvisation."},
                    "brand_preset": {"type": "string", "description": "Brand preset shipped inside Chaos (e.g. 'molecule')."},
                    "filter": {"type": "string", "description": "Feature filter for the features/correlations sections, auto-detected as folder | layer | topic (same routing as chaos_features) — e.g. 'desci-infra' scopes to that folder."},
                    "title": {"type": "string"},
                    "slug": {"type": "string"},
                    "output_html": {"type": "string", "description": "Override the default docs/features_memory/<slug>-composed.html path."},
                    "limit": {"type": "integer", "default": 0, "description": "Cap features in the features section; 0 = all."},
                    "feature_pages": {"type": "boolean", "default": false, "description": "SITE MODE: also write one page per feature under <slug>-composed/ and make the index's feature cards CLICKABLE links to them. Each per-feature page shows the feature's code/files, its quotient-graph relations to the rest of the stack (Solidity neighbours tagged as smart contracts, in-scope neighbours cross-linked), prior overlapping pages, and a deterministic persona-adapted walkthrough built ONLY from indexed data (the page says so). Every per-feature page embeds its own chaos-composed-manifest with its own content_hash and is individually hash-gated — unchanged features are never rewritten, and the return reports written vs cached counts so you do not re-ingest unchanged pages."}
                },
                "required": ["repo", "sections"]
            }
        }),
        json!({
            "name": "chaos_project",
            "description": "Manage CROSS-REPOSITORY projects: a named set of indexed repos (client, backend, contracts, infra, …). Chaos detects feature→feature CROSS-REPO LINKS between members from the persisted index (consumer → provider): `package_dep` (imports a package another member publishes), `abi` (references a Solidity contract defined elsewhere), `http_route` (a fetch/axios call path matches a registered route — anchored on persisted HttpRoute surface nodes unioned with the chunk scan), `graphql` (an executable GraphQL operation selects a root field another member's SDL schema defines; operation types must agree — SDL-derived schemas only, code-first servers produce no provider facet yet). Links attach at the feature (L1) level with evidence + provenance, and refresh AUTOMATICALLY after chaos_analyze/chaos_add on any member (hash-gated — a no-change re-index relinks nothing). Actions: create (idempotent), add_repo (attach an INDEXED repo under an alias; links immediately), add_docs (index a directory of project-level DOCS — cross-repo design notes, ADRs, migration spikes — as a docs-only member; the dir may sit ABOVE the member repos: nested member repos are pruned from the walk, and re-running on the same dir is an idempotent refresh), list (also returns EVERY indexed repository — the discovery call when you don't know what Chaos already knows; a sub-app inside one indexed repo is a chaos_features folder/layer filter, not a project), status (members, staleness, links by kind, embedder consistency), relink (`force` overrides the gate). Use chaos_features with `project` for the cross-repo feature inventory.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["create", "add_repo", "add_docs", "list", "status", "relink"], "description": "What to do."},
                    "project": {"type": "string", "description": "Project name (required for every action except list)."},
                    "repo": {"type": "string", "description": "Repository path or name to attach (add_repo)."},
                    "dir": {"type": "string", "description": "Directory of project-level documentation to index as a docs-only member (add_docs; markdown/PDF). May be the project root — nested member repos are pruned from the walk."},
                    "alias": {"type": "string", "description": "Project-scoped alias for the repo (client/backend/contracts/infra/…). Defaults to the repo name (add_repo) or \"docs\" (add_docs)."},
                    "force": {"type": "boolean", "default": false, "description": "relink: re-detect even when no member's root hash moved."}
                },
                "required": ["action"]
            }
        }),
        json!({
            "name": "chaos_feature_story",
            "description": "Tell the cross-repo STORY of one feature across a PROJECT (a named set of indexed repos). Given a `project` and a free-text `feature`, it matches that feature in EVERY member repo (L1 community semantic search + a lexical label fallback), loads the persisted cross-repo links and TRAVERSES them — pulling in a link's other endpoint (e.g. the Solidity contract a client calls) even when the query didn't match it directly — then orders the involved features into a journey-layer SPINE (entry → interface → core → foundation = client → backend → contracts). Writes a CLICKABLE MULTI-PAGE SITE to the project workspace: an index page (the spine + the cross-repo link chain + repos not involved) and one drill-down page per involved feature (its code/files, its cross-repo links cross-linked + smart-contract tagged, prior overlapping pages, a deterministic walkthrough). Every page embeds a `chaos-feature-story-manifest` with its own content_hash and is individually hash-gated — an unchanged page is never rewritten and the return reports written vs cached counts. Deterministic and embedder-LIGHT (one embed for the whole query, reused across repos); needs the embedder for cross-repo matching. Returns a COMPACT JSON summary (involved repos one-liners, the ordered link chain, links_by_kind, not-involved repos, the site summary, the index output_html, provenance, content_hash) — narrate the spine for the user; full detail lives in the HTML. Use it to answer 'how does feature X work across every repo?'. Distinct from chaos_features --project (an inventory of ALL features) and chaos_compose (one repo's composed page).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": {"type": "string", "description": "Project name — a set of indexed repos (see chaos_project). The story spans every member."},
                    "feature": {"type": "string", "description": "The feature to trace across repos, e.g. 'lab tokenization and access control'."},
                    "style": {"type": "string", "description": "Style preset: editorial (default light) | blade-runner (dark neon). Unknown = error."},
                    "brand_preset": {"type": "string", "description": "Brand preset shipped inside Chaos (e.g. 'molecule')."},
                    "output_html": {"type": "string", "description": "Override the default <workspace>/<slug>-story.html index path."},
                    "limit": {"type": "integer", "default": 0, "description": "Cap on matched features per repo; 0 = the default."}
                },
                "required": ["project", "feature"]
            }
        }),
        json!({
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
        }),
        json!({
            "name": "chaos_gaps",
            "description": "List KNOWLEDGE GAPS — code retrieval cannot find. Two kinds: coverage_gaps (files that produced NO chunks — invisible to every retrieval method; re-add them, report a chunking bug if they stay empty) and vocabulary_gaps (chunked code whose indexed text carries too little DISTINCTIVE vocabulary to match any meaningful query: single-letter names, abbreviation soup, no docstrings). Corpus-driven and deterministic (background vocabulary is derived from each repo's own document frequencies, not a hardcoded stop list), read-only, embedder-free, COMPACT return with per-file evidence samples and a `next` instruction. Pass `repo` for one repository (optionally `folder` to scope flagging to a sub-app path inside a monorepo-indexed repo), or `project` to scan EVERY member repo of a cross-repo project in one repo-tagged report. The FIX for a vocabulary gap is repo content — ask the user what the file is for, write a file-top docstring or folder README capturing it, then run chaos_add with those paths; NEVER block or pause indexing waiting for the answer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "Indexed repository path or name (omit when passing project)."},
                    "project": {"type": "string", "description": "Cross-repo project name — scans every member repo."},
                    "folder": {"type": "string", "description": "Path prefix inside the repo to scope flagging to (repo mode only)."}
                }
            }
        }),
        json!({
            "name": "chaos_graph",
            "description": "Export an already-indexed repository as a standalone interactive HTML graph (the full L0 node/edge view) read from the persisted index — embedder-free, writes one self-contained file. Defaults to docs/features_memory/graph.html inside the repo (so chaos_clean --artifacts sweeps it); override with `output`. The static page's search box is a SUBSTRING filter; for LIVE semantic search in the page (a human validation surface running the same hierarchical retrieval pipeline as chaos_query), tell the user to run `chaos graph <repo> --serve` in a terminal — a long-running local server, deliberately not an MCP tool. For the feature-level map (L1 communities + quotient graph) use chaos_obsidian / chaos_refresh instead, which write feature-map.html.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string"},
                    "output": {"type": "string", "description": "Output HTML path. Defaults to <repo>/docs/features_memory/graph.html."}
                },
                "required": ["repo"]
            }
        }),
        json!({
            "name": "chaos_usage",
            "description": "Find WHO CONSUMES a symbol or surface string across the repo, grouped by subfolder — the cross-folder 'who uses this?' answer served entirely from the persisted index, so you NEVER fall back to rg/grep over the target repo. Given a `target` (a function/method/struct name, an env-var name, an HTTP header or route string, …) it gathers every use site from three EMBEDDER-FREE sources: (1) user-surface nodes (env_var / http_route / cli_command / graphql_field) named exactly the target — a bare GraphQL field name additionally matches qualified graphql_field nodes by suffix, so target `user` finds `Query.user` — AST-extracted real reads/registrations, no false hits from comments, and every consumer across folders shares the same name; (2) REVERSE GRAPH EDGES — the target resolves to its definition node(s) and the persisted graph yields who calls/imports/uses_type/implements/tests/depends_on it, cross-file (index-backed); (3) a LITERAL chunk sweep for the exact string, the cross-language catch-all for references that aren't first-class nodes (e.g. an x-service-token header), sampled ≤2 hits/file. Sites are deduped by (file, line) — structured graph sites win over a literal hit at the same spot — and grouped by top-level subfolder (most-used first), each tagged with the MECHANISM (reads env var / calls / imports / registers route / defines GraphQL field / references (literal)) and language. ALWAYS writes an interactive HTML report to docs/features_memory/<slug>-usage.html (manifest embedded under id=\"chaos-usage-manifest\") and returns a COMPACT JSON summary (target, definition sites, per-folder site counts + capped one-line sites with sites_omitted, PROVENANCE breadcrumbs, the HTML path) so it won't flood your context. Read-only and embedder-free — runs even when no embedder is configured. HONEST LIMITATION (surfaced as a warning): call/import edges resolve cross-file only when the symbol name is repo-unique, so ambiguously-named consumers may be undercounted; the literal sweep backstops but samples per file. Use it to answer 'who uses X / where is this consumed across the monorepo's subfolders' without grepping.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string"},
                    "target": {"type": "string", "description": "The symbol / env-var name / HTTP header / route string whose consumers to find (matched exactly for nodes; substring for the literal sweep)."},
                    "output_html": {"type": "string", "description": "Override the default docs/features_memory/<slug>-usage.html path."},
                    "limit": {"type": "integer", "default": 6, "description": "Use sites listed per subfolder in the compact return (the HTML holds them all)."}
                },
                "required": ["repo", "target"]
            }
        }),
    ]
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
            let summary =
                crate::pipeline::run_analyze(config, storage, embedder, Path::new(repo_path))
                    .await?;
            Ok(tool_text(serde_json::to_string_pretty(&summary)?))
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
        "chaos_pages" => {
            let repo = args
                .get("repo")
                .and_then(Value::as_str)
                .context("repo is required")?;
            let opts = crate::pages::PagesOptions {
                features_dir: args
                    .get("features_dir")
                    .and_then(Value::as_str)
                    .map(PathBuf::from),
            };
            let summary = crate::pages::run(storage, repo, &opts).await?;
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
            let answer =
                crate::pipeline::run_query(storage, embedder, repo, question, limit, hierarchical)
                    .await?;
            Ok(tool_text(serde_json::to_string_pretty(&answer)?))
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
            let opts = crate::feature_context::FeatureContextOptions {
                features_dir: args
                    .get("features_dir")
                    .and_then(Value::as_str)
                    .map(PathBuf::from),
                output_html: args
                    .get("output_html")
                    .and_then(Value::as_str)
                    .map(PathBuf::from),
                limit: args.get("limit").and_then(Value::as_i64).unwrap_or(0),
                feature_limit: args
                    .get("feature_limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize,
                nodes_per_feature: args
                    .get("nodes_per_feature")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize,
            };
            // ALWAYS writes the HTML (full evidence + extractable JSON); returns a
            // compact pointer-only summary so the agent's context is not flooded.
            let summary = crate::feature_context::run(storage, embedder, repo, task, &opts).await?;
            Ok(tool_text(serde_json::to_string_pretty(&summary)?))
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
        "chaos_sui_migration_impact" => {
            let repo = args
                .get("repo")
                .and_then(Value::as_str)
                .context("repo is required")?;
            let opts = crate::sui_migration::SuiMigrationOptions {
                source: args.get("source").and_then(Value::as_str).map(String::from),
                output_html: args
                    .get("output_html")
                    .and_then(Value::as_str)
                    .map(PathBuf::from),
                features_dir: args
                    .get("features_dir")
                    .and_then(Value::as_str)
                    .map(PathBuf::from),
                limit: args.get("limit").and_then(Value::as_u64).unwrap_or(12) as usize,
            };
            let summary = crate::sui_migration::run(storage, repo, &opts).await?;
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
            let output = args
                .get("output")
                .and_then(Value::as_str)
                .map(PathBuf::from);
            let summary = crate::pipeline::run_obsidian(storage, repo, output).await?;
            Ok(tool_text(serde_json::to_string_pretty(&summary)?))
        }
        "chaos_refresh" => {
            let repo = args
                .get("repo")
                .and_then(Value::as_str)
                .context("repo is required")?;
            let obsidian_output = args
                .get("obsidian_output")
                .and_then(Value::as_str)
                .map(PathBuf::from);
            let features_dir = args
                .get("features_dir")
                .and_then(Value::as_str)
                .map(PathBuf::from);
            let all_features = args
                .get("all_features")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let summary = crate::pipeline::run_refresh(
                storage,
                repo,
                obsidian_output,
                features_dir,
                all_features,
            )
            .await?;
            Ok(tool_text(serde_json::to_string_pretty(&summary)?))
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
        "chaos_usage" => {
            let repo = args
                .get("repo")
                .and_then(Value::as_str)
                .context("repo is required")?;
            let target = args
                .get("target")
                .and_then(Value::as_str)
                .context("target is required")?;
            let opts = crate::usage::UsageOptions {
                output_html: args
                    .get("output_html")
                    .and_then(Value::as_str)
                    .map(PathBuf::from),
                limit: args.get("limit").and_then(Value::as_u64).unwrap_or(0) as usize,
            };
            // Embedder-free: literal sweep + reverse graph edges + surface nodes.
            let summary = crate::usage::run(storage, repo, target, &opts).await?;
            Ok(tool_text(serde_json::to_string_pretty(&summary)?))
        }
        "chaos_compose" => {
            let repo = args
                .get("repo")
                .and_then(Value::as_str)
                .context("repo is required")?;
            let sections = args
                .get("sections")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let opts = crate::compose::ComposeOptions {
                sections,
                persona: args
                    .get("persona")
                    .and_then(Value::as_str)
                    .map(String::from),
                level: args.get("level").and_then(Value::as_str).map(String::from),
                style: args.get("style").and_then(Value::as_str).map(String::from),
                brand_preset: args
                    .get("brand_preset")
                    .and_then(Value::as_str)
                    .map(String::from),
                filter: args.get("filter").and_then(Value::as_str).map(String::from),
                title: args.get("title").and_then(Value::as_str).map(String::from),
                slug: args.get("slug").and_then(Value::as_str).map(String::from),
                output_html: args
                    .get("output_html")
                    .and_then(Value::as_str)
                    .map(PathBuf::from),
                limit: args.get("limit").and_then(Value::as_u64).unwrap_or(0) as usize,
                feature_pages: args
                    .get("feature_pages")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            };
            // Explicit level / no persona stays embedder-free.
            let summary = crate::compose::run(storage, Some(embedder), repo, &opts).await?;
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
                curation: args
                    .get("curation")
                    .map(|v| serde_json::from_value(v.clone()))
                    .transpose()
                    .context("invalid `curation` — expected {groups: [{title, icon?, blurb?, features: [{label, note?}]}]}")?,
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
            let action = args.get("action").and_then(Value::as_str).context(
                "action is required: create | add_repo | add_docs | list | status | relink",
            )?;
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
                "add_docs" => {
                    let dir = args
                        .get("dir")
                        .and_then(Value::as_str)
                        .context("dir is required (the docs directory to index)")?;
                    let alias = args.get("alias").and_then(Value::as_str);
                    crate::project::add_docs(
                        storage,
                        embedder,
                        &config.indexing,
                        name()?,
                        Path::new(dir),
                        alias,
                    )
                    .await?
                }
                "list" => crate::project::list(storage).await?,
                "status" => crate::project::status(storage, name()?).await?,
                "relink" => {
                    let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
                    crate::project::relink(storage, name()?, force).await?
                }
                other => anyhow::bail!(
                    "unknown action `{other}` — use create | add_repo | add_docs | list | status | relink"
                ),
            };
            Ok(tool_text(serde_json::to_string_pretty(&summary)?))
        }
        "chaos_feature_story" => {
            let project = args
                .get("project")
                .and_then(Value::as_str)
                .context("project is required")?;
            let feature = args
                .get("feature")
                .and_then(Value::as_str)
                .context("feature is required")?;
            let opts = crate::feature_story::FeatureStoryOptions {
                style: args.get("style").and_then(Value::as_str).map(String::from),
                brand_preset: args
                    .get("brand_preset")
                    .and_then(Value::as_str)
                    .map(String::from),
                output_html: args
                    .get("output_html")
                    .and_then(Value::as_str)
                    .map(PathBuf::from),
                limit: args.get("limit").and_then(Value::as_u64).unwrap_or(0) as usize,
            };
            let summary =
                crate::feature_story::run(storage, embedder, project, feature, &opts).await?;
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
        "chaos_gaps" => {
            if let Some(project) = args.get("project").and_then(Value::as_str) {
                let report = crate::gaps::build_project_gaps_report(storage, project).await?;
                return Ok(tool_text(serde_json::to_string_pretty(&report)?));
            }
            let repo = args
                .get("repo")
                .and_then(Value::as_str)
                .context("pass repo (one repository) or project (every member repo)")?;
            let repo = storage
                .find_repository(repo)
                .await?
                .context("repository is not indexed")?;
            let folder = args.get("folder").and_then(Value::as_str);
            let report =
                crate::gaps::build_gaps_report(storage, repo.id, &repo.name, folder).await?;
            Ok(tool_text(serde_json::to_string_pretty(&report)?))
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
                    features_memory_dir(Path::new(&repo.root_path)).join("graph.html")
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
                "edges": graph.edges.len(),
                "semantic_search": format!(
                    "static page = substring filter only; for live semantic search run: chaos graph {} --serve",
                    repo.root_path
                ),
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
  what's the stack   chaos_stack {repo}  — declared dependencies, scripts, CDK stacks/resources, configs, exposed API surface (HTTP routes, SDL-derived GraphQL root fields, CLI commands), languages — LISTED, with explicit coverage notes (embedder-free)
  what's extracted   chaos_pages {repo}  — list the generated pages (kind, title, modified) — use INSTEAD of ls/globbing docs/features_memory (embedder-free)
  knowledge gaps     chaos_gaps {repo | project, folder?}  — code retrieval can't find: no chunks at all, or no distinctive vocabulary after identifier splitting; project scans every member repo, folder scopes to a sub-app; fix = file-top docstring or folder README, then chaos_add those paths — never pause indexing on it (embedder-free)
  ask a question     chaos_query {repo, question, hierarchical: true}  — feature-routed retrieval; flat search without the flag
  grasp a big area   chaos_components {repo, area?}  — curated component overview with a read order (run BEFORE feature work)
  list features      chaos_features {repo | project, filter?}  — exhaustive inventory; filter auto-detects folder | layer (exact word OR by meaning: 'backend', 'client app') | topic; after composing your answer, call again with curation {groups: [{title, icon?, blurb?, features: [{label, note?}]}]} so the HTML carries your human domains + notes
  scope a change     chaos_change_plan {repo, change_description, since?}  — which features a change spans, in check order
  who uses X         chaos_usage {repo, target}  — cross-folder consumers of a symbol/env-var/header/route/GraphQL-field from the index (surface nodes incl. graphql_field + reverse edges + literal sweep; bare field matches Query.field by suffix), grouped by subfolder — use INSTEAD of rg/grep over the repo (embedder-free)
  gather evidence    chaos_feature_context {repo, task}  — implementation context (COMPACT return: ranked evidence lines with the top hits' bodies inlined as code_excerpt; full evidence + verbatim code in the written output_html under id=chaos-feature-context-data); READ the actual body before any behavioral claim; treat its warnings as blockers; FINISH the drill-down with chaos_write_feature_website so the explanation persists as a page
  impact (before)    chaos_impact {repo, feature}  — how a proposed feature maps onto today's code, compact return + HTML
  sui migration      chaos_sui_migration_impact {repo, source?}  — map each feature onto Sui primitives (objects/PTBs/events) + Walrus/Seal, embedder-free, compact return + HTML
  document (eng)     chaos_write_feature_website {repo, slug, title, manifest}  — OMIT html: Chaos renders the page from the manifest; manifest.purpose (REQUIRED) opens the page with what the feature was made for, manifest.examples[] add a clickable 'How you'd use it' section
  document (users)   chaos_write_storyboard {repo, slug, title, manifest}  — code-free feature guide for stakeholders
  compose a page     chaos_compose {repo, sections: [features|correlations|stack], persona?|level?, style?, filter?, feature_pages?}  — THE surface for any user-facing webpage/website request: ONE page (or a SITE: feature_pages=true adds one hash-gated, cross-linked page per feature with code/files, stack relations [smart contracts tagged], persona-fitted walkthrough); persona routes by meaning; style: editorial | blade-runner; brand_preset: molecule; everything content-hashed (cached: true = you already hold it, do NOT re-ingest); if it fails, REPORT the failure — never fake the page with shell tools
  cross-repo         chaos_project {action: create | add_repo | add_docs | list | status | relink}  — link client/backend/contracts/infra repos (link kinds: package_dep | abi | http_route | graphql); add_docs indexes a project-level docs dir (ADRs, design notes) as a docs-only member; then chaos_features {project}
  cross-repo story   chaos_feature_story {project, feature}  — how ONE feature works across EVERY repo: matches it per repo, follows the cross-repo links, orders client → backend → contracts, writes a clickable multi-page site (hash-gated); narrate the returned spine
  exports            chaos_obsidian / chaos_refresh  — regenerate vault + pages from the index, no embedder
  graph view         chaos_graph {repo, output?}  — standalone interactive L0 node/edge HTML (feature map comes from obsidian/refresh)
  fresh start        chaos_clean {repo?, artifacts?, confirm: true}  — DESTRUCTIVE index wipe (one repo or all); artifacts also deletes generated files

RULES OF THUMB
  - Index before anything else; chaos_add after each change keeps memory fresh (hash-gated: unchanged content costs zero embedder calls).
  - Returns are compact excerpts (chunk text capped, lists capped); the generated HTML under docs/features_memory/ keeps FULL evidence.
  - Compose feature pages from chaos_feature_context evidence, never from chaos_query alone; pass manifests, never raw HTML.
  - A feature DEEP-DIVE is not done until the page exists: end it with chaos_write_feature_website (engineers) or chaos_write_storyboard (stakeholders) — an explanation that lives only in chat is lost when the session ends.
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
    let slug = crate::export_util::safe_slug(slug, "feature-context");
    let output = features_memory_dir(Path::new(repo_root)).join(format!("{slug}-explanation.html"));
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let manifest_json = serde_json::to_string_pretty(manifest)?;
    let manifest_block = format!(
        r#"<script type="application/json" id="chaos-feature-manifest">
{}
</script>"#,
        escape_script_json(&manifest_json)
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
        html_escape_full(title),
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
    let parsed: crate::feature_context::FeatureManifest =
        serde_json::from_value(value).map_err(|err| {
            anyhow::anyhow!(
                "manifest does not match the FeatureManifest schema: {err}. Field shapes: purpose is a plain STRING; examples[] are {{title, description, steps[], code, language, node_ids}}; nodes[].evidence and edges[].evidence are OBJECTS {{source, method, notes}} (not strings); claims need {{id, title, body, confidence, node_ids}}; modes {{id, title, node_ids}}; edges {{source, target, label}}; story steps {{id, title, body, node_ids}}"
            )
        })?;
    let slug = crate::export_util::safe_slug(slug, "feature-context");
    let output = features_memory_dir(Path::new(repo_root)).join(format!("{slug}-explanation.html"));
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
    // The page must open with WHY the feature exists, not just what it
    // contains — a reader should never have to reverse-engineer the purpose
    // from the graph. (Older pages on disk without it still parse; this gate
    // applies to new writes only.)
    let purpose_len = manifest
        .get("purpose")
        .and_then(Value::as_str)
        .map(str::trim)
        .map(str::len)
        .unwrap_or(0);
    if purpose_len == 0 {
        anyhow::bail!(
            "manifest.purpose must explain in plain language what this feature was made for (who uses it, what problem it solves); it renders as the page's opening band. Add a simple usage example in manifest.examples too when the feature has a callable surface."
        );
    }
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
            "purpose": "Explains what the sample feature is for.",
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
        for tool in TOOL_NAMES {
            if tool == "chaos_help" {
                continue;
            }
            assert!(AGENT_GUIDE.contains(tool), "guide missing {tool}");
        }
    }

    #[test]
    fn tools_list_matches_tool_names() {
        let defs = tool_definitions();
        let listed: Vec<&str> = defs
            .iter()
            .map(|def| def["name"].as_str().expect("every tool has a name"))
            .collect();
        assert_eq!(
            listed,
            TOOL_NAMES.to_vec(),
            "tools/list roster must equal TOOL_NAMES, in order"
        );
        for def in &defs {
            let name = def["name"].as_str().unwrap();
            assert!(
                def["description"].as_str().is_some_and(|d| !d.is_empty()),
                "{name} is missing a description"
            );
            assert!(
                def["inputSchema"].is_object(),
                "{name} is missing an inputSchema"
            );
        }
    }

    /// The CLI subcommand a `chaos_*` MCP tool corresponds to; None for the
    /// one deliberately MCP-only tool (its manifest arrives inline from the
    /// agent — there is no sensible file-based CLI shape for it).
    fn cli_twin(tool: &str) -> Option<String> {
        match tool {
            "chaos_write_feature_website" => None,
            // Same renderer; the CLI reads the storyboard manifest from disk.
            "chaos_write_storyboard" => Some("storyboard".into()),
            other => Some(other.trim_start_matches("chaos_").replace('_', "-")),
        }
    }

    #[test]
    fn every_tool_has_a_cli_twin() {
        use clap::CommandFactory;
        let cli = crate::Cli::command();
        let subcommands: Vec<String> = cli
            .get_subcommands()
            .map(|sub| sub.get_name().to_string())
            .collect();
        for tool in TOOL_NAMES {
            let Some(twin) = cli_twin(tool) else {
                continue;
            };
            assert!(
                subcommands.contains(&twin),
                "MCP tool {tool} expects the CLI twin `{twin}`, which clap does not know"
            );
        }
    }

    #[test]
    fn every_cli_command_has_an_mcp_twin() {
        use clap::CommandFactory;
        // CLI-only surfaces: ops plumbing (migrate/doctor/setup/hook/mcp),
        // the agent guide (help — chaos_help returns the MCP-flavored guide),
        // file-manifest rendering (storyboard — chaos_write_storyboard is the
        // inline-manifest twin), and hidden debug spikes.
        const CLI_ONLY: [&str; 9] = [
            "migrate",
            "doctor",
            "setup",
            "hook",
            "help",
            "mcp",
            "storyboard",
            "struct-features",
            "communities",
        ];
        let cli = crate::Cli::command();
        for sub in cli.get_subcommands() {
            let name = sub.get_name().to_string();
            if CLI_ONLY.contains(&name.as_str()) {
                continue;
            }
            let tool = format!("chaos_{}", name.replace('-', "_"));
            assert!(
                TOOL_NAMES.contains(&tool.as_str()),
                "CLI command `{name}` has no MCP twin `{tool}` — add the tool or list the command as CLI-only"
            );
        }
    }

    /// Test-only dispatch probe. It FAILS instead of fabricating vectors (the
    /// FailEmbedder precedent) — the probed calls never reach embedding.
    struct DispatchProbeEmbedder;

    #[async_trait::async_trait]
    impl Embedder for DispatchProbeEmbedder {
        fn provider(&self) -> &'static str {
            "probe"
        }
        fn model_id(&self) -> &str {
            "probe"
        }
        fn dimensions(&self) -> usize {
            768
        }
        async fn embed(&self, _input: &str) -> Result<Vec<f32>> {
            anyhow::bail!("embedder unavailable (dispatch probe)")
        }
    }

    /// Arguments that carry each tool PAST dispatch but stop at argument
    /// validation — or, where every argument is optional, at the first
    /// (deliberately unreachable) database touch.
    fn dispatch_probe_args(tool: &str) -> Value {
        match tool {
            // repo_path defaults to "." — point it away from the working tree
            // so the probe never walks this repo; it fails fast at the first
            // storage call against the lazy never-connecting pool.
            "chaos_add" => json!({"repo_path": "/nonexistent/chaos-dispatch-probe"}),
            _ => json!({}),
        }
    }

    #[tokio::test]
    async fn every_tool_name_is_dispatched() {
        let config = Config {
            storage: crate::config::StorageConfig {
                database_url: "postgres://probe:probe@127.0.0.1:9/probe".into(),
            },
            embedding: crate::config::EmbeddingConfig {
                provider: crate::config::EmbeddingProvider::Ollama,
                model: "probe".into(),
                dimensions: 768,
                base_url: None,
            },
            indexing: Default::default(),
        };
        let storage = Storage::connect_lazy_for_tests(&config.storage.database_url)
            .expect("lazy pool needs no server");
        for tool in TOOL_NAMES {
            let outcome = handle_tool_call(
                tool,
                dispatch_probe_args(tool),
                &config,
                &storage,
                &DispatchProbeEmbedder,
            )
            .await;
            if let Err(err) = outcome {
                assert!(
                    !format!("{err:#}").contains("unknown tool"),
                    "TOOL_NAMES entry {tool} is not dispatched: {err:#}"
                );
            }
        }
        let err = handle_tool_call(
            "chaos_not_a_tool",
            json!({}),
            &config,
            &storage,
            &DispatchProbeEmbedder,
        )
        .await
        .expect_err("unknown tool names must be rejected");
        assert!(err.to_string().contains("unknown tool"));
    }

    fn repo_file(rel: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
        std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("reading {rel}: {err}"))
    }

    #[test]
    fn every_tool_documented_in_every_doc() {
        for doc in [
            "README.md",
            "RUNBOOK.md",
            "ARCHITECTURE.md",
            "docs/EDITOR_SETUP.md",
            "AGENTS.md",
            "CLAUDE.md",
            "skills/chaos-substrate/SKILL.md",
        ] {
            let content = repo_file(doc);
            for tool in TOOL_NAMES {
                assert!(content.contains(tool), "{doc} does not mention {tool}");
            }
        }
    }

    #[test]
    fn doc_tool_counts_match_tool_names_len() {
        fn spelled(n: usize) -> Option<&'static str> {
            match n {
                22 => Some("twenty-two"),
                23 => Some("twenty-three"),
                24 => Some("twenty-four"),
                25 => Some("twenty-five"),
                26 => Some("twenty-six"),
                _ => None,
            }
        }
        let n = TOOL_NAMES.len();
        let word = spelled(n).expect("teach spelled() the new roster size");
        let stale: Vec<String> = (20..=30)
            .filter(|count| *count != n)
            .flat_map(|count| {
                let mut phrases = vec![format!("{count} tools"), format!("{count} MCP tools")];
                if let Some(w) = spelled(count) {
                    phrases.push(w.to_string());
                }
                phrases
            })
            .collect();
        for doc in [
            "README.md",
            "RUNBOOK.md",
            "ARCHITECTURE.md",
            "docs/EDITOR_SETUP.md",
        ] {
            let content = repo_file(doc);
            for phrase in &stale {
                assert!(
                    !content.contains(phrase.as_str()),
                    "{doc} carries the stale tool count {phrase:?}; the roster holds {n}"
                );
            }
            assert!(
                content.contains(word)
                    || content.contains(&format!("{n} tools"))
                    || content.contains(&format!("{n} MCP tools")),
                "{doc} never states the tool count ({n} / {word})"
            );
        }
    }

    #[test]
    fn plugin_manifest_versions_match_cargo() {
        let version = env!("CARGO_PKG_VERSION");
        for manifest in [".claude-plugin/plugin.json", ".codex-plugin/plugin.json"] {
            let parsed: Value = serde_json::from_str(&repo_file(manifest))
                .unwrap_or_else(|err| panic!("parsing {manifest}: {err}"));
            assert_eq!(
                parsed["version"].as_str(),
                Some(version),
                "{manifest} version"
            );
        }
        let marketplace: Value =
            serde_json::from_str(&repo_file(".claude-plugin/marketplace.json"))
                .expect("parsing marketplace.json");
        let entry = marketplace["plugins"]
            .as_array()
            .and_then(|plugins| plugins.iter().find(|p| p["name"] == "chaos-substrate"))
            .expect("marketplace.json lists the chaos-substrate plugin");
        assert_eq!(
            entry["version"].as_str(),
            Some(version),
            ".claude-plugin/marketplace.json plugin-entry version"
        );
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
            "purpose": "Lets a signed-in user prove who they are before touching protected routes.",
            "examples": [{
                "title": "Sign in and call a protected route",
                "description": "The happy path a new integrator follows.",
                "steps": ["POST /login", "Send the returned token as a Bearer header"],
                "code": "curl -H 'Authorization: Bearer <token>' /api/me",
                "language": "sh",
                "node_ids": ["n1", "n2"]
            }],
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
        // Purpose + example survive into the embedded manifest the page renders from.
        assert!(html.contains("Lets a signed-in user prove who they are"));
        assert!(html.contains("Sign in and call a protected route"));

        // A manifest without a purpose is rejected: the page must open with
        // what the feature was made for.
        let mut no_purpose = manifest.clone();
        no_purpose.as_object_mut().unwrap().remove("purpose");
        let err = write_manifest_feature_website(
            dir.path().to_str().unwrap(),
            "no-purpose",
            "No purpose",
            &no_purpose,
        )
        .expect_err("purpose is required");
        assert!(err.to_string().contains("manifest.purpose"));

        // Thin manifests are still rejected (the evidence contract holds).
        let thin = json!({"purpose": "p", "claims": [], "modes": [], "nodes": [], "edges": [], "story": []});
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
