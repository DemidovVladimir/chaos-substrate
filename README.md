# Chaos Substrate

**A portable, persistent code-knowledge memory for AI agents that survives process restarts.**

Chaos Substrate indexes a repository into a source-grounded knowledge graph plus real embedding
vectors stored in **Postgres + pgvector**, so agents stop re-reading the whole repo on every task
and instead get fast, source-grounded answers from durable memory. It is a single Rust binary
(`chaos`) that exposes that memory through a CLI and a stdio MCP server.

The implementation is Rust-only. It analyzes Rust, TypeScript, JavaScript, Python, and Solidity with
**real AST parsers**, treats Markdown/MDX and text PDFs as supplemental document context, and reads a
small set of JSON config manifests. Non-Rust languages are analysis targets only — they are parsed
Rust-side, never run as a separate Node or Python service. It can also export a standalone
`graph.html` page or an Obsidian vault for visual validation of the persisted graph.

## Why it exists

- **Persistent memory.** The graph, chunks, and embeddings live in your Postgres/pgvector and
  survive process restarts — queries after a restart use disk-backed memory, not a re-scan.
- **Source-grounded answers.** Graph nodes and edges stay canonical for source grounding and context
  routing; chunks are symbol-aware retrieval projections, not the source of truth.
- **Fail-closed.** Runtime code has no mock embedder and no random vectors. If no real embedder is
  configured, analysis fails rather than producing fake vectors.

## Two ways to use

| Mode | What | For | How to start |
| --- | --- | --- | --- |
| **Agent via MCP** | A stdio MCP server with 17 tools (`chaos_analyze`, `chaos_add`, `chaos_stats`, `chaos_query`, `chaos_feature_context`, `chaos_impact`, `chaos_write_feature_website`, `chaos_obsidian`, `chaos_refresh`, `chaos_write_storyboard`, `chaos_change_plan`, `chaos_components`, `chaos_features`, `chaos_project`, `chaos_help`, `chaos_clean`, `chaos_graph`). | Coding agents (Claude Code, Codex, Cursor, Windsurf, OpenCode) that should query durable code memory instead of re-reading files. | `chaos setup` to register the server, then ask the agent to analyze and query. See [docs/EDITOR_SETUP.md](docs/EDITOR_SETUP.md). |
| **Raw CLI** | The `chaos` binary: `analyze`, `add`, `stats`, `query`, `feature-context`, `impact`, `change-plan`, `storyboard`, `graph`, `obsidian`, `refresh`, `clean`. | Humans and scripts doing setup, debugging, one-off indexing, or agentless operation. | `chaos analyze <repo>` then `chaos query <repo> "<question>"`. See [Quick Start](#quick-start). |
| **Generated static feature-website** | A self-contained HTML feature page (light editorial theme) with interactive graph/story/code navigation plus a machine-readable manifest. | Sharing or reviewing how a feature works, and seeding future agent context from the embedded manifest. | `chaos feature-context <repo> "<task>" --output-html page.html`, or the `chaos_write_feature_website` MCP tool. |
| **Client/user storyboard** | A self-contained HTML page (light editorial theme) that explains a feature from the UI/UX user-story perspective with **no code**: personas, "As a … I want … so that …" stories, clickable frames, confidence rings, an embedded manifest, and optional **real-UI previews** per frame (a captured screenshot/clip or a live `iframe` of the running app). | Handing a stakeholder or end user an interactive presentation of a feature without showing code. | The `chaos_write_storyboard` MCP tool, or `chaos storyboard <repo> --manifest story.json`. |

## Quick Start

This is the canonical bootstrap. Bundled Postgres uses `pgvector/pgvector:pg16` on host port `54329`.

```bash
cp chaos-substrate.example.toml chaos-substrate.toml   # example config defaults to local Ollama
docker compose up -d                                   # pgvector on localhost:54329
# Ollama default: ensure Ollama is running and the model is pulled:
#   ollama pull embeddinggemma   (see docs/OLLAMA_SETUP.md)
chaos migrate                                           # create schema (sqlx migrations)
chaos doctor                                            # check Postgres + real embedding probe
chaos analyze /path/to/repo                             # index the repository
chaos query /path/to/repo "where is the request handler validated?"
# After editing a few files, index just the diff + regenerate docs in one shot:
chaos add /path/to/repo -m "added the export pipeline"  # detects changed files from git
```

The default `DATABASE_URL` for the bundled container is
`postgres://chaos:chaos@localhost:54329/chaos_substrate`.

> **New to the project and using Claude Code?** Follow
> [docs/QUICKSTART_CLAUDE.md](docs/QUICKSTART_CLAUDE.md) for the full path from installing Rust
> to generating a feature page, as a single linear guide.

The example config defaults to local Ollama (`embeddinggemma`, 768 dims,
`http://localhost:11434`). Ollama must be running and the model pulled before `chaos doctor` will
pass. See [docs/OLLAMA_SETUP.md](docs/OLLAMA_SETUP.md) for install, model pull, and
troubleshooting.

**Using OpenAI instead:** uncomment the `open_ai` block in `chaos-substrate.toml`, comment out the
`ollama` block, and set `OPENAI_API_KEY` in your environment (`export OPENAI_API_KEY=...`). OpenAI
uses `text-embedding-3-small` (1536 dims).

> The CLI examples above use the installed `chaos` binary. During development you can substitute
> `cargo run --` for any one-off CLI command (for example `cargo run -- analyze /path/to/repo`).
> Do **not** use `cargo run` in MCP/plugin configuration — launch the release binary directly.

## Install for your editor

Register Chaos Substrate as an MCP server with one command:

```bash
chaos setup                 # auto-detect Claude Code / Codex / Cursor / Windsurf / OpenCode
chaos setup --dry-run       # show planned changes without writing
chaos setup --scope project # write a shareable project-scoped config
```

`chaos setup` merges (does not clobber) existing MCP configuration in each detected editor and points
it at the release binary over stdio. There is also `chaos hook`, a Claude Code / Cursor plugin hook
that reads a `PreToolUse`/`PostToolUse` event on stdin and injects code-memory context for
`Grep`/`Glob`/`Bash`; it always exits 0 and is a safe no-op when the DB or index is unavailable (no
embedder dependency). The plugin ships `.claude-plugin/hooks/hooks.json` and `.cursor/hooks.json`.

Per-editor instructions (Claude Code, Codex, Cursor, Windsurf, OpenCode) live in
**[docs/EDITOR_SETUP.md](docs/EDITOR_SETUP.md)**.

MCP/plugin config must launch the release binary directly over stdio:

```bash
cargo build --release
./target/release/chaos --config chaos-substrate.toml mcp
```

## MCP Tools

The stdio MCP server speaks newline-delimited JSON-RPC (no `Content-Length` framing) and exposes
exactly seventeen tools. **This is the canonical tool reference.**

| Tool | What it does | Key params | When to use |
| --- | --- | --- | --- |
| `chaos_help` | Returns the agent guide: recommended tool order, typical workflows (index → query → orient → scope → document → cross-repo), and token notes (returns are excerpts; generated HTML keeps full evidence). Static text — no DB or embedder work. | _none_ | Once when first meeting the server, or whenever unsure which tool fits. |
| `chaos_clean` | **Destructive.** Wipes the persisted index — one repo (`repo`) or everything (omit it); `artifacts: true` also deletes generated files on disk (vault, feature pages, project workspaces). Requires `confirm: true`; reports exactly what was removed. Schema survives; the index stays empty until a re-index is requested. | `repo`, `artifacts`, `confirm` (required) | Starting truly clean before a re-validation, on explicit user request only. |
| `chaos_graph` | Exports the indexed repo as a standalone interactive HTML graph (the full L0 node/edge view) from the persisted index. Embedder-free. Defaults to `docs/features_memory/graph.html` in the repo (so `chaos_clean --artifacts` sweeps it). The L1 feature map (`feature-map.html`) comes from `chaos_obsidian`/`chaos_refresh` instead. | `repo`, `output` | Visually validating the persisted graph after an analyze, without shelling out to the CLI. |
| `chaos_analyze` | Indexes or refreshes a repository into the persistent graph + embeddings. | `repo_path` | First, to build or update memory for a repo before querying. |
| `chaos_add` | Incrementally indexes the files changed in git (or explicit `paths`), refreshes the Obsidian vault, and writes an interactive feature/bug page — in one shot. | `repo_path`, `paths`, `since`, `kind` (`feature`\|`bug`), `message`, `obsidian_output`, `no_obsidian`, `no_page` | After changing a few files, to update memory + docs without a full re-index. Auto-classifies feature vs bug from git. |
| `chaos_stats` | Reports index statistics for an already-indexed repository, read from Postgres: totals (files, nodes, edges, chunks, embedded vs missing, split chunks) plus breakdowns of nodes by kind, edges by kind, chunks by type, and files by language. | `repo` | After `chaos_analyze`/`chaos_add`, to explain or sanity-check what was indexed. Read-only and embedder-free. |
| `chaos_query` | Answers a focused, source-grounded question via hybrid (semantic + keyword) retrieval. With `hierarchical`, retrieves top-down — matching feature (community) summaries first and returning the surfaced features alongside the chunk hits, falling back to flat search when no hierarchy exists. | `repo`, `question`, `limit` (default 10), `hierarchical` | To get a grounded answer about specific code without re-reading files. |
| `chaos_feature_context` | Gathers evidence for understanding a feature: semantic/keyword hits, graph context, feature-page manifests. | `repo`, `task`, `limit` (10), `feature_limit` (3), `nodes_per_feature` (8), `features_dir`, `output_html` | Before implementing or explaining a feature, to assemble an implementation brief. Pass `output_html` to also write the feature page. |
| `chaos_impact` | Builds a feature-vs-existing-code impact report for an indexed repo and **always** writes an interactive HTML (impact summary + evidence dashboard) to `docs/features_memory/<slug>-impact.html`; returns a compact JSON summary (counts, the existing files/symbols the feature touches, warnings, and the HTML path) while the full evidence stays in the HTML. | `repo`, `feature`, `features_dir`, `output_html`, `limit` (10), `feature_limit` (3), `nodes_per_feature` (8) | Before implementing a feature, to see how it maps onto the codebase as it is today (the "before") without flooding context like a raw `chaos_feature_context` dump. |
| `chaos_write_feature_website` | Writes an engineer-facing feature page plus its machine-readable manifest. **Pass the manifest only (omit `html`)** — Chaos renders the interactive page deterministically (same renderer as `chaos add`), so the LLM never authors raw HTML; an explicit `html` argument remains as a legacy path. | `repo`, `slug`, `title`, `manifest`, `html` (legacy, optional) | To persist a reviewed feature explanation as a shareable static page. |
| `chaos_obsidian` | Exports an already-indexed repository as an Obsidian vault (one Markdown note per graph node, grouped into topic notes, plus an edge manifest) read from the persisted graph. | `repo`, `output` | After `chaos_analyze` (which never writes files), to materialize the persisted graph as a browsable vault. Defaults `output` to `<repo>/chaos-obsidian-vault`. |
| `chaos_refresh` | Regenerates project-local artifacts from the persisted index without re-indexing: rewrites the Obsidian vault and, with `all_features`, re-renders the deterministic feature pages from their embedded manifests. | `repo`, `obsidian_output`, `features_dir`, `all_features` | After `chaos_analyze` or `chaos_add`, to refresh generated docs without paying for a full re-index. |
| `chaos_write_storyboard` | Writes a client/user-facing storyboard — a code-free UI/UX user-story page (personas, "As a … I want … so that …" stories, clickable frames, outcomes, confidence rings) in the shared light editorial theme to `docs/features_memory/<slug>-story.html`, with an embedded `chaos-storyboard-manifest`. You pass a structured, code-free manifest; Rust owns the styling. Each frame can embed the **real UI** via an optional `preview` (a screenshot/clip, or a live `iframe`). | `repo`, `slug`, `title`, `manifest` | To hand a stakeholder/end user an interactive presentation of a feature with no code. User-facing sibling of `chaos_write_feature_website`. |
| `chaos_change_plan` | Decomposes a proposed change into the **features** (L1 communities / god-nodes) it spans, with a dependency-aware check order. Matches the change description against community summary embeddings (optionally also seeding from a real git diff via `since`), then **always** writes an interactive HTML plan (light editorial theme) to `docs/features_memory/<slug>-plan.html` and returns a compact JSON summary (per-feature label, confidence, check order, top symbols, HTML path). | `repo`, `change`, `since` | Before implementing a change, to scope which features it touches and in what order to verify them. |
| `chaos_components` | Explains the **core components** of a big area (the step *before* feature extraction). An area like "OCL" spans several L1 communities; given an `area` (or none, for a repo-level overview) it surfaces those communities as components — each with its summary, key symbols/files, languages, and a quotient-graph role (entry/interface/core/foundation) — plus how they connect and a dependency-first read order. **Always** writes an interactive HTML overview to `docs/features_memory/<slug>-components.html` (with an embedded `chaos-components-manifest`) and returns a compact JSON summary. | `repo`, `area`, `output_html`, `limit` (8), `top_members` (12) | To understand a large subsystem and its constituent components before drilling into any single feature. |
| `chaos_features` | Lists **all god-node features** (L1 communities) that match a filter, grouped by journey layer (entry → interface → core → foundation). The exhaustive, uncurated counterpart to `chaos_components`. The single `filter` is **auto-detected**: a path/real directory → **folder** scope; a layer word (`client`/`ui`/`api`/`core`/`contracts`) → that **layer** (so "client features" = every entry-layer feature); anything else → a **topic** match; omit it for the whole repo. Force it with `layer`/`folder`/`topic`. Only a topic filter needs the embedder. **Always** writes an interactive HTML inventory to `docs/features_memory/<slug>-features.html` (embedded `chaos-features-manifest`) and returns a compact JSON summary (resolved filter, per-layer + language counts, per-feature label/role/folders/symbols/matched_by, provenance). | `repo` or `project`, `filter`, `layer`, `folder`, `topic`, `output_html`, `limit` (0 = all) | To inventory every feature in a layer/folder/topic — e.g. "give me all client features". With `project`, lists EVERY member repo's features in one journey-layered inventory (repo-tagged, cross-link-annotated, written to the project workspace). |
| `chaos_project` | Manages **cross-repository projects** — a named set of indexed repos (client, backend, smart contracts, infra, …). Detects **feature→feature cross-repo links** between members from the persisted index (consumer → provider): `package_dep` (one repo imports a package the other publishes), `abi` (code references a Solidity contract defined in another repo), `http_route` (a fetch/axios call path matches a route registered elsewhere). Links attach at the feature (L1) level with evidence + provenance, and refresh **automatically** after `chaos_analyze`/`chaos_add` on any member (gated by the L2 repo root hash — a no-change re-index relinks nothing). | `action` (`create`\|`add_repo`\|`list`\|`status`\|`relink`), `project`, `repo`, `alias`, `force` | To group client/backend/contracts/infra repos and see how features connect across them; then `chaos_features` with `project` for the cross-repo inventory. |

Agents should prefer MCP tools when available, and should not synthesize feature pages from
`chaos_query` alone when `chaos_feature_context` and `chaos_write_feature_website` are available. The
writer rejects README-like pages: feature pages must include interactive graph/story/code
navigation, architecture/flow sections, evidence, and a populated manifest. If
`chaos_feature_context` returns warnings about missing indexed subtrees or missing documentation
hits, refresh or re-target the context before writing a feature website.

## Supported languages

All non-Rust extraction uses **real AST parsers**, not regex or pattern matching. Each captures
functions, classes/structs, interfaces/traits, enums, type aliases, class methods, imports,
inheritance edges (Rust traits/impls and Solidity only), and (heuristic, file-scoped) call edges.

| Language | Parser | Functions | Classes/Structs | Methods | Imports | Inheritance | Calls |
| --- | --- | :---: | :---: | :---: | :---: | :---: | :---: |
| Rust | `syn` | ✅ | ✅ | ✅ | ✅ | ✅ (traits/impls) | ✅ |
| TypeScript / JavaScript | `oxc` | ✅ | ✅ | ✅ | ✅ | ➖ | ✅ |
| Python | `rustpython-parser` | ✅ | ✅ | ✅ | ✅ | ➖ | ✅ |
| Solidity | `solang-parser` | ✅ | ✅ (contracts/libraries) | ✅ | ✅ | ✅ | ✅ |

Supplemental context:

- **Markdown / MDX** and **text PDF** are indexed as document context at lower retrieval and graph
  weight than source code.
- **JSON** is limited to config manifests: `package.json`, `cdk.json`, `tsconfig.json`,
  `jsconfig.json` (plus `Cargo.toml`, parsed for dependency nodes). AWS CDK apps/stacks/resources are
  extracted from CDK config.

Honest residual limits: no `tsc`/type inference, no path-alias resolution, and cross-file call
resolution is name-based.

## Security & data

- **Your data stays in your database.** The graph, chunks, and embeddings live in *your* Postgres +
  pgvector instance (bundled `docker-compose.yml` runs `pgvector/pgvector:pg16` locally on port
  `54329`). Nothing is sent to a third party beyond your chosen embedding provider.
- **Embeddings require a real provider.** OpenAI (`text-embedding-3-small`, 1536 dims) or Ollama
  (`embeddinggemma`, 768 dims). Only chunk text is sent to the embedder.
- **Fail-closed by design.** There are no mock embedders and no random vectors. If no real embedder
  is available, analysis fails rather than fabricating data. A dimension check prevents incompatible
  vectors from being stored.
- **Rust-side extraction only.** TypeScript/JavaScript, Python, and Solidity are parsed in-process by
  the Rust binary. No Node or Python runtime service is spawned.

## Storage

Postgres tables: `repositories`, `analysis_runs`, `files`, `nodes`, `edges`, `chunks`, `embeddings`.
Migrations run via `sqlx::migrate!` and are tracked in `_sqlx_migrations`. The `embeddings` table
stores provider, model, dimensions, content hash, and pgvector data.

## CLI

```bash
chaos help [<command>]                                 # agent-friendly guide: commands, workflows, examples (no DB/config needed)
chaos migrate                                          # create/update schema
chaos doctor                                           # check Postgres + embedding provider
chaos clean [<repo>] [--artifacts]                     # wipe the index (all repos, or one); --artifacts also deletes generated files
chaos analyze <repo>                                   # index a repository
chaos add <repo> [-m "<what changed>"]                 # index git-diff + refresh vault + write feature/bug page
chaos stats <repo>                                     # report index statistics (read-only, embedder-free)
chaos query <repo> "<question>" [--limit N] [--hierarchical]   # source-grounded answer (top-down with --hierarchical)
chaos feature-context <repo> "<task>" [--output-html page.html]
chaos impact <repo> "<feature>"                        # feature-vs-existing-code impact report + HTML
chaos change-plan <repo> "<change>" [--since <ref>]    # decompose a change into features + check order + HTML plan
chaos components <repo> ["<area>"]                     # explain a big area's core components (overview before features)
chaos features [<repo>] ["<filter>"] [--project <name>]   # list ALL features (folder | layer | topic auto-detect; --project spans repos)
chaos project create|add-repo|list|status|relink       # cross-repo projects (client + backend + contracts + infra)
chaos storyboard <repo> --manifest story.json          # render a client-facing user-story page (no code)
chaos graph <repo> [-o graph.html]                     # export interactive graph page
chaos obsidian <repo> [-o vault]                       # export Obsidian vault
chaos refresh <repo> [--all-features]                  # regenerate project-local artifacts
chaos setup [--dry-run] [--scope user|local|project]   # register MCP server in editors
chaos hook --event <PreToolUse|PostToolUse> [--format claude|cursor]
chaos mcp                                              # run the stdio MCP server
```

Global flag: `--config <PATH>` (default `chaos-substrate.toml`). `DATABASE_URL` overrides the config.
Diagnostics go to stderr (tracing); program results go to stdout.

`doctor` checks Postgres and performs a real embedding probe against the configured provider.

`analyze` extracts files, functions, classes, interfaces, type aliases, enums, structs, traits,
impls, modules, tests, source line ranges, `contains`/`imports`/`depends_on`/`calls` graph edges,
symbol-aware chunks linked back to graph nodes, and real embeddings for every chunk. It also pulls
Cargo dependencies, `package.json` dependencies/scripts, tsconfig/jsconfig files, and AWS CDK
apps/stacks/resources, and indexes Markdown/MDX docs and extractable text PDFs as supplemental
context.

`graph` exports a self-contained `graph.html` for visual validation of the persisted nodes and edges
— pan, zoom, filter by node kind, search, and click nodes for source metadata. It does not run a web
server, require Node.js, or call an embedding provider.

`obsidian` exports the persisted graph as a local Obsidian vault (`Topics/`, `Nodes/`, `Edges.md`,
`README.md`, `.obsidian/`). It only reads the persisted graph.

`refresh` is the after-reindex command for project-local generated artifacts; it reads the persisted
graph and regenerates the repository Obsidian vault (and, with `--all-features`, feature pages).

`feature-context` builds an implementation brief: semantic and keyword hits from Postgres, graph
context paths, relevant generated feature pages, and feature metadata (claims, graph modes, nodes,
edges, story-step scopes, evidence, confidence). Generated feature websites embed a
`<script type="application/json" id="chaos-feature-manifest">` block as the stable machine contract;
`feature-context` scans direct `*.html` files in `docs/features_memory` by default and ignores pages
without this manifest.

## Development / Docs index

| Doc | Purpose |
| --- | --- |
| [ARCHITECTURE.md](ARCHITECTURE.md) | System design: extraction, storage, retrieval, and the MCP surface. |
| [RUNBOOK.md](RUNBOOK.md) | Canonical ops command reference for running, indexing, and maintaining. |
| [CHANGELOG.md](CHANGELOG.md) | Release notes — what changed in each version and what to know when upgrading. |
| [llms.txt](llms.txt) | Machine-readable project summary for LLMs. |
| [docs/QUICKSTART_CLAUDE.md](docs/QUICKSTART_CLAUDE.md) | End-to-end onboarding for Claude Code: Rust install → bootstrap → plugin → index → feature page. |
| [docs/EDITOR_SETUP.md](docs/EDITOR_SETUP.md) | Canonical per-editor install (Claude Code / Codex / Cursor / Windsurf / OpenCode). |
| [docs/MCP_SETUP.md](docs/MCP_SETUP.md) | Generic stdio MCP server setup and JSON-RPC details. |
| [docs/CLAUDE_MCP_INSTALL.md](docs/CLAUDE_MCP_INSTALL.md) | Registering the server with Claude Code / Claude Desktop. |
| [docs/CLAUDE_CODE_COWORK.md](docs/CLAUDE_CODE_COWORK.md) | Claude Code and Cowork plugin setup. |
| [docs/PLUGIN_INSTALL.md](docs/PLUGIN_INSTALL.md) | Codex and Claude plugin installation. |
| [docs/plugin-install.html](docs/plugin-install.html) | Dark visual plugin-install tutorial. |
| [docs/PLUGIN_WORKFLOW.md](docs/PLUGIN_WORKFLOW.md) | Plugin wrapper workflow and natural-language intents. |
| [docs/FEATURE_CONTEXT.md](docs/FEATURE_CONTEXT.md) | Feature-context agent workflow and manifest contract. |
| [docs/GRAPH_WEBPAGE.md](docs/GRAPH_WEBPAGE.md) | `graph.html` export setup and validation tutorial. |
| [docs/OBSIDIAN_EXPORT.md](docs/OBSIDIAN_EXPORT.md) | Obsidian vault export workflow. |
| [docs/REFRESH_EXPORTS.md](docs/REFRESH_EXPORTS.md) | `refresh` command reference for generated artifacts. |
| [docs/OLLAMA_SETUP.md](docs/OLLAMA_SETUP.md) | Ollama install, model pull, and embedding troubleshooting. |
| [docs/TYPESCRIPT_JAVASCRIPT_SUPPORT.md](docs/TYPESCRIPT_JAVASCRIPT_SUPPORT.md) | TypeScript/JavaScript extraction details and limits. |
| [docs/RUST_EXTRACTOR_NOTES.md](docs/RUST_EXTRACTOR_NOTES.md) | Rust (`syn`) extractor implementation notes. |
| [docs/STORAGE_SCHEMA_REVIEW.md](docs/STORAGE_SCHEMA_REVIEW.md) | Postgres schema review and rationale. |
| [docs/AGENT_VALIDATION.md](docs/AGENT_VALIDATION.md) | Agent-facing validation checklist. |
| [docs/CLAUDE_VALIDATION_BRIEF.md](docs/CLAUDE_VALIDATION_BRIEF.md) | Claude validation brief. |

### Validation

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

For real repository indexing, configure either OpenAI or Ollama embeddings. If the embedder is
unavailable, analysis must fail rather than producing fake vectors.

### Key source files

`src/main.rs` (clap CLI), `src/mcp.rs` (MCP server, 17 tools), `src/config.rs` (toml+env config),
`src/storage.rs` (Postgres, sqlx), `src/embedding.rs` (OpenAI/Ollama embedders), `src/extractor.rs`
(orchestration + Rust/Cargo/Markdown/PDF/JSON/AWS-CDK extraction + call edges),
`src/lang/{mod,javascript,python,solidity}.rs` (oxc/rustpython/solang AST extraction),
`src/weights.rs` (edge cost/confidence), `src/query.rs` (hybrid retrieval),
`src/community.rs` + `src/merkle.rs` + `src/community_summary.rs` + `src/change_plan.rs` +
`src/hierarchy_export.rs` (hierarchical memory: L1 Louvain god-nodes, L2 Merkle rollup,
L3 hash-gated community summaries, change-plan tool, and god-node/feature-map export),
`src/feature_context.rs` + `src/feature_export.rs` (feature pages), `src/graph_export.rs`,
`src/obsidian_export.rs`, `src/setup.rs`, `src/hook.rs`, `src/export_util.rs`, and
`migrations/001_init.sql` (plus `002_communities.sql`, `003_subtree_hash.sql`,
`004_community_summary.sql`).
