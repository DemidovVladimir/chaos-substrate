# Chaos Substrate

**A portable, persistent code-knowledge memory for AI agents that survives process restarts.**

Chaos Substrate indexes a repository into a source-grounded knowledge graph plus real embedding
vectors stored in **Postgres + pgvector**, so agents stop re-reading the whole repo on every task
and instead get fast, source-grounded answers from durable memory. It is a single Rust binary
(`chaos`) that exposes that memory through a CLI and a stdio MCP server.

The implementation is Rust-only. It analyzes Rust, TypeScript, JavaScript, Python, Solidity, and
GraphQL (standalone `.graphql`/`.gql`/`.graphqls` files plus `gql`-embedded documents in TS/JS and
Python) with **real AST parsers**, treats Markdown/MDX and text PDFs as supplemental document
context, and reads a small set of JSON config manifests. Non-Rust languages are analysis targets
only — they are parsed Rust-side, never run as a separate Node or Python service. It can also export a standalone
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
| **Agent via MCP** | A stdio MCP server with 24 tools (`chaos_analyze`, `chaos_add`, `chaos_stats`, `chaos_stack`, `chaos_pages`, `chaos_gaps`, `chaos_query`, `chaos_feature_context`, `chaos_impact`, `chaos_usage`, `chaos_sui_migration_impact`, `chaos_write_feature_website`, `chaos_obsidian`, `chaos_refresh`, `chaos_write_storyboard`, `chaos_change_plan`, `chaos_components`, `chaos_features`, `chaos_compose`, `chaos_project`, `chaos_feature_story`, `chaos_help`, `chaos_clean`, `chaos_graph`). | Coding agents (Claude Code, Codex, Windsurf, OpenCode) that should query durable code memory instead of re-reading files. | `chaos setup` to register the server, then ask the agent to analyze and query. See [docs/EDITOR_SETUP.md](docs/EDITOR_SETUP.md). |
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

> **New to the project and using Claude Code?** The
> [Setup from scratch](#setup-from-scratch-claude-code--codex) section below walks the full path
> from installing Docker and Rust to registering the agent, in order.

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

## Setup from scratch (Claude Code & Codex)

Everything needed on a fresh machine, in order. Nothing here assumes Docker, Rust, or an editor
integration is already installed.

**1. Install Docker** — runs the bundled Postgres + pgvector.

```bash
# macOS:  brew install --cask docker   (or download Docker Desktop from docker.com)
# Linux:  https://docs.docker.com/engine/install/  (Docker Engine + the compose plugin)
docker compose version    # verify — must succeed before continuing
```

**2. Install Rust** — the runtime is a single Rust binary built with `cargo`.

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
cargo --version           # verify
```

**3. Clone and bootstrap** — one command builds the binary, starts Postgres, installs and starts
Ollama (pulling `embeddinggemma`), runs migrations, and health-checks everything:

```bash
git clone https://github.com/chaos-substrate/chaos-substrate.git
cd chaos-substrate
cp chaos-substrate.example.toml chaos-substrate.toml   # defaults to local Ollama (no API key)
bin/chaos bootstrap
export PATH="$HOME/.local/bin:$PATH"                   # puts `chaos` on PATH
```

Prefer OpenAI embeddings? Before bootstrap, uncomment the `open_ai` block in
`chaos-substrate.toml`, comment out the `ollama` block, and `export OPENAI_API_KEY=...`.

**4. Register with Claude Code** — either the full plugin (skills + MCP tools + hooks) or the
MCP server alone:

```bash
# Full plugin (local testing):
claude --plugin-dir "$(pwd)"
# Full plugin (real install): add .claude-plugin/marketplace.json and install from the /plugin UI.

# MCP server only:
bin/chaos claude-code-add local                              # private, machine-local
bin/chaos claude-code-add project /abs/path/to/target-repo   # shareable .mcp.json in a target repo
```

**5. Register with Codex** — plugin marketplace or MCP server alone:

```bash
# Plugin (skills + MCP):
codex plugin marketplace add "$(pwd)"      # reads .agents/plugins/marketplace.json
# then restart Codex and enable chaos-substrate from the plugin UI

# MCP server only:
codex mcp add chaos-substrate -- "$(pwd)/target/release/chaos" --config "$(pwd)/chaos-substrate.toml" mcp
```

**6. Verify** — the editor should list all twenty-four `chaos_*` tools; then index a repo:

```bash
chaos doctor                          # Postgres + pgvector + real embedder probe
chaos analyze /path/to/your/repo      # build the persistent memory
chaos query /path/to/your/repo "where is auth handled?"
```

Per-editor install (Claude Code, Codex, Windsurf, OpenCode), the plugin packages (marketplace /
Cowork zip), Claude Desktop, and hooks: [docs/EDITOR_SETUP.md](docs/EDITOR_SETUP.md).

## Install for your editor

Register Chaos Substrate as an MCP server with one command:

```bash
chaos setup                 # auto-detect Claude Code / Codex / Windsurf / OpenCode
chaos setup --dry-run       # show planned changes without writing
chaos setup --scope project # write a shareable project-scoped config
```

`chaos setup` merges (does not clobber) existing MCP configuration in each detected editor and points
it at the release binary over stdio. There is also `chaos hook`, a Claude Code plugin hook
that reads a `PreToolUse`/`PostToolUse` event on stdin and injects code-memory context for
`Grep`/`Glob`/`Bash`; it always exits 0 and is a safe no-op when the DB or index is unavailable (no
embedder dependency). The plugin ships `.claude-plugin/hooks/hooks.json`.

Per-editor instructions (Claude Code, Codex, Windsurf, OpenCode) live in
**[docs/EDITOR_SETUP.md](docs/EDITOR_SETUP.md)**.

MCP/plugin config must launch the release binary directly over stdio:

```bash
cargo build --release
./target/release/chaos --config chaos-substrate.toml mcp
```

## MCP Tools

The stdio MCP server speaks newline-delimited JSON-RPC (no `Content-Length` framing) and exposes
exactly twenty-four tools. **This is the canonical tool reference.**

| Tool | What it does | Key params | When to use |
| --- | --- | --- | --- |
| `chaos_help` | Returns the agent guide: recommended tool order, typical workflows (index → query → orient → scope → document → cross-repo), and token notes (returns are excerpts; generated HTML keeps full evidence). Static text — no DB or embedder work. | _none_ | Once when first meeting the server, or whenever unsure which tool fits. |
| `chaos_clean` | **Destructive.** Wipes the persisted index — one repo (`repo`) or everything (omit it); `artifacts: true` also deletes generated files on disk (vault, feature pages, project workspaces). Requires `confirm: true`; reports exactly what was removed. Schema survives; the index stays empty until a re-index is requested. | `repo`, `artifacts`, `confirm` (required) | Starting truly clean before a re-validation, on explicit user request only. |
| `chaos_graph` | Exports the indexed repo as a standalone interactive HTML graph (the full L0 node/edge view) from the persisted index. Embedder-free. Defaults to `docs/features_memory/graph.html` in the repo (so `chaos_clean --artifacts` sweeps it). The L1 feature map (`feature-map.html`) comes from `chaos_obsidian`/`chaos_refresh` instead. | `repo`, `output` | Visually validating the persisted graph after an analyze, without shelling out to the CLI. |
| `chaos_analyze` | Indexes or refreshes a repository into the persistent graph + embeddings. | `repo_path` | First, to build or update memory for a repo before querying. |
| `chaos_add` | Incrementally indexes the files changed in git (or explicit `paths`), refreshes the Obsidian vault, and writes an interactive feature/bug page — in one shot. | `repo_path`, `paths`, `since`, `kind` (`feature`\|`bug`), `message`, `obsidian_output`, `no_obsidian`, `no_page` | After changing a few files, to update memory + docs without a full re-index. Auto-classifies feature vs bug from git. |
| `chaos_stats` | Reports index statistics for an already-indexed repository, read from Postgres: totals (files, nodes, edges, chunks, embedded vs missing, split chunks) plus breakdowns of nodes by kind, edges by kind, chunks by type, and files by language. | `repo` | After `chaos_analyze`/`chaos_add`, to explain or sanity-check what was indexed. Read-only and embedder-free. |
| `chaos_stack` | Reports the **tech stack** of an already-indexed repository, listed rather than counted: manifest-declared dependencies by ecosystem (npm/cargo — versions, runtime-vs-dev scope, how many workspace manifests declare each, widest-declared first), npm scripts, deployment resources (AWS CDK app entrypoints, Stack classes, L2 constructs by cloud service), indexed JS/TS configs, the repo's exposed **API surface** from persisted user-surface nodes (HTTP routes with method + path, GraphQL root fields grouped `Query.`/`Mutation.`/`Subscription.` — SDL-derived only — and CLI commands), and the language breakdown. **Always** writes an interactive HTML inventory to `docs/features_memory/stack.html` (embedded `chaos-stack-manifest`) and returns a compact JSON summary with explicit **coverage notes** (what the index does not extract yet: Dockerfiles, CI workflows, pyproject.toml, foundry.toml, Terraform, code-first GraphQL schemas). | `repo`, `output_html` | To answer "what is this repo built with / what infrastructure does it use / what API does it expose?" without grepping manifests. Read-only and embedder-free. |
| `chaos_pages` | Lists the **generated feature-memory pages** of a repository — what chaos has already extracted. Scans `docs/features_memory` (or `features_dir`) and returns every HTML page with its kind (`feature`/`story`/`components`/`features`/`composed`/`stack`/`impact`/`change-plan`/`feature-map`; unrecognised files appear as `other`, never hidden), the tool that writes that kind, its title, and its modified time, newest first, plus by-kind counts. Pure filesystem — works even when the repo row is missing, as long as the path exists. | `repo`, `features_dir` | To check whether a feature was already extracted before starting a new deep-dive, and to find the page to reopen — instead of `ls`/globbing the docs directory. Read-only, embedder-free, DB-optional. |
| `chaos_gaps` | Lists the **knowledge gaps** — code retrieval cannot find. Two kinds: `coverage_gaps` (files that produced **no** chunks — invisible to every retrieval method) and `vocabulary_gaps` (chunked, but carrying almost no distinctive vocabulary after identifier splitting; background words are corpus-derived, never a hardcoded list). Pass `repo` for one repository, `repo` + `folder` for a sub-app inside a monorepo-indexed repo, or `project` to scan every member repo in one repo-tagged report. Compact return with per-file evidence samples and a `next` instruction. The fix for a vocabulary gap is repo content: a file-top docstring or folder README, then `chaos_add` those paths — never pause indexing on it. | `repo` or `project`, `folder` | To audit what the index can never find after an analyze, or when retrieval misses a file the user insists is relevant. Read-only and embedder-free. |
| `chaos_query` | Answers a focused, source-grounded question via hybrid (semantic + keyword) retrieval. With `hierarchical`, retrieves top-down — matching feature (community) summaries first and returning the surfaced features alongside the chunk hits, falling back to flat search when no hierarchy exists. | `repo`, `question`, `limit` (default 10), `hierarchical` | To get a grounded answer about specific code without re-reading files. |
| `chaos_feature_context` | Gathers evidence for understanding a feature: semantic/keyword hits, graph context, feature-page manifests. | `repo`, `task`, `limit` (10), `feature_limit` (3), `nodes_per_feature` (8), `features_dir`, `output_html` | Before implementing or explaining a feature, to assemble an implementation brief. Always writes a compact evidence HTML (override the path with `output_html`); the return is a compact payload whose top hits carry a bounded inline `code_excerpt` — the full evidence stays in the HTML. |
| `chaos_impact` | Builds a feature-vs-existing-code impact report for an indexed repo and **always** writes an interactive HTML (impact summary + evidence dashboard) to `docs/features_memory/<slug>-impact.html`; returns a compact JSON summary (counts, the existing files/symbols the feature touches, warnings, and the HTML path) while the full evidence stays in the HTML. | `repo`, `feature`, `features_dir`, `output_html`, `limit` (10), `feature_limit` (3), `nodes_per_feature` (8) | Before implementing a feature, to see how it maps onto the codebase as it is today (the "before") without flooding context (the full evidence stays in the HTML). |
| `chaos_usage` | Reports **who consumes** a symbol or surface string (env var, HTTP header, route, GraphQL field, function) across the repo, grouped by top-level subfolder — the cross-folder "who uses this?" answer from the persisted index (user-surface `env_var`/`http_route`/`cli_command`/`graphql_field` nodes + reverse graph edges `calls`/`imports`/`uses_type`/`implements`/`tests`/`depends_on` + a literal chunk sweep; a bare GraphQL field name matches qualified `graphql_field` nodes by suffix, so `user` finds `Query.user`). Read-only and embedder-free. **Always** writes `docs/features_memory/<slug>-usage.html` (embedded `chaos-usage-manifest`) and returns a compact per-folder JSON summary (capped lists with `sites_omitted` counts). Honest limitation surfaced as a warning: call/import edges resolve cross-file only for repo-unique names. | `repo`, `target`, `output_html` | To answer "who uses this symbol / env var / route across the codebase?" without grepping the target repo. |
| `chaos_sui_migration_impact` | Produces a Sui migration impact report for an indexed Ethereum, Solana, or mixed Web3 repo — auto-detects the source stack, maps each L1 feature onto Sui primitives (objects/dynamic fields, Coin/Kiosk/Display, capabilities, PTBs, events+GraphQL) with Walrus/Seal storage and access-control verdicts, each citing the compiled-in Sui official docs profile. Read-only, embedder-free. **Always** writes `docs/features_memory/sui-migration-impact.html` and returns a compact JSON summary. Maps impact only — does not generate Move code. | `repo`, `source` (auto\|ethereum\|solana\|mixed), `output_html`, `features_dir`, `limit` (12) | To plan a migration of an Ethereum/Solana codebase to Sui: see which features are affected, how they map to Sui primitives, and which need Walrus/Seal integration. |
| `chaos_write_feature_website` | Writes an engineer-facing feature page plus its machine-readable manifest. **Pass the manifest only (omit `html`)** — Chaos renders the interactive page deterministically (same renderer as `chaos add`), so the LLM never authors raw HTML; an explicit `html` argument remains as a legacy path. The manifest's required `purpose` opens the page with a plain-language "what this was made for" band, and `examples` render a clickable "How you'd use it" section that highlights the code path on the graph. | `repo`, `slug`, `title`, `manifest`, `html` (legacy, optional) | To persist a reviewed feature explanation as a shareable static page that explains purpose and usage, not just structure. |
| `chaos_obsidian` | Exports an already-indexed repository as an Obsidian vault (one Markdown note per graph node, grouped into topic notes, plus an edge manifest) read from the persisted graph. | `repo`, `output` | After `chaos_analyze` (which never writes files), to materialize the persisted graph as a browsable vault. Defaults `output` to `<repo>/chaos-obsidian-vault`. |
| `chaos_refresh` | Regenerates project-local artifacts from the persisted index without re-indexing: rewrites the Obsidian vault and, with `all_features`, re-renders the deterministic feature pages from their embedded manifests. | `repo`, `obsidian_output`, `features_dir`, `all_features` | After `chaos_analyze` or `chaos_add`, to refresh generated docs without paying for a full re-index. |
| `chaos_write_storyboard` | Writes a client/user-facing storyboard — a code-free UI/UX user-story page (personas, "As a … I want … so that …" stories, clickable frames, outcomes, confidence rings) in the shared light editorial theme to `docs/features_memory/<slug>-story.html`, with an embedded `chaos-storyboard-manifest`. You pass a structured, code-free manifest; Rust owns the styling. Each frame can embed the **real UI** via an optional `preview` (a screenshot/clip, or a live `iframe`). | `repo`, `slug`, `title`, `manifest` | To hand a stakeholder/end user an interactive presentation of a feature with no code. User-facing sibling of `chaos_write_feature_website`. |
| `chaos_change_plan` | Decomposes a proposed change into the **features** (L1 communities / god-nodes) it spans, with a dependency-aware check order. Matches the change description against community summary embeddings (optionally also seeding from a real git diff via `since`), then **always** writes an interactive HTML plan (light editorial theme) to `docs/features_memory/<slug>-plan.html` and returns a compact JSON summary (per-feature label, confidence, check order, top symbols, HTML path). | `repo`, `change`, `since` | Before implementing a change, to scope which features it touches and in what order to verify them. |
| `chaos_components` | Explains the **core components** of a big area (the step *before* feature extraction). An area like "OCL" spans several L1 communities; given an `area` (or none, for a repo-level overview) it surfaces those communities as components — each with its summary, key symbols/files, languages, and a quotient-graph role (entry/interface/core/foundation) — plus how they connect and a dependency-first read order. **Always** writes an interactive HTML overview to `docs/features_memory/<slug>-components.html` (with an embedded `chaos-components-manifest`) and returns a compact JSON summary. | `repo`, `area`, `output_html`, `limit` (8), `top_members` (12) | To understand a large subsystem and its constituent components before drilling into any single feature. |
| `chaos_features` | Lists **all god-node features** (L1 communities) that match a filter, grouped by journey layer (entry → interface → core → foundation). The exhaustive, uncurated counterpart to `chaos_components`. The single `filter` is **auto-detected**: a path/real directory → **folder** scope; a layer word (`client`/`ui`/`api`/`core`/`contracts`) → that **layer** (so "client features" = every entry-layer feature); any other phrase is first tried as a layer **by meaning** (embedding match — "backend", "client app", "devops" resolve semantically; "backend" spans interface+core), then falls to a **topic** match; omit it for the whole repo. Force it with `layer`/`folder`/`topic`. Exact layer words and folders are embedder-free. **Always** writes an interactive HTML inventory to `docs/features_memory/<slug>-features.html` (embedded `chaos-features-manifest`) and returns a compact JSON summary (resolved filter, per-layer + language counts, per-feature label/role/folders/symbols/matched_by, provenance). | `repo` or `project`, `filter`, `layer`, `folder`, `topic`, `output_html`, `limit` (0 = all) | To inventory every feature in a layer/folder/topic — e.g. "give me all client features". With `project`, lists EVERY member repo's features in one journey-layered inventory (repo-tagged, cross-link-annotated, written to the project workspace). |
| `chaos_compose` | **Composes ONE page from knowledge-base-backed sections** instead of several similar pages: `features` (inventory with each feature's concise L3 explanation), `correlations` (files shared between those features + prior generated pages that overlap them), `stack` (declared dependencies/scripts/deploy resources). Tailored to an **audience** — free-text `persona` resolved to beginner/practitioner/expert **by meaning** (prototype embeddings, no keyword list) or an explicit `level` (embedder-free) — and a **style preset** (`editorial` light default, `blade-runner` dark neon) plus optional `brand_preset` (e.g. `molecule`). Uses **only** the persisted index and prior manifests — never parses source files; an unservable section is a **loud error**, never a fallback. The composition is **content-hashed**: re-composing the same request over unchanged knowledge returns `cached: true` without writing, so agents don't re-ingest a memory they already hold. Writes `docs/features_memory/<slug>-composed.html` (embedded `chaos-composed-manifest`). **Site mode** (`feature_pages`): one extra hash-gated page per feature under `<slug>-composed/` — clickable from the index, cross-linked to in-scope neighbours, showing code/files, stack relations (Solidity neighbours tagged smart contracts), prior overlapping pages, and a deterministic persona-adapted walkthrough. This is the default surface for any user-facing webpage/website request. | `repo`, `sections`, `persona`, `level`, `style`, `brand_preset`, `filter`, `title`, `slug`, `output_html`, `limit`, `feature_pages` | To hand someone (a beginner, an expert, a stakeholder) one tailored page — "features under desci-infra with explanations and correlations, beginner persona, blade-runner style" — instead of stitching several standalone pages. |
| `chaos_project` | Manages **cross-repository projects** — a named set of indexed repos (client, backend, smart contracts, infra, …). Detects **feature→feature cross-repo links** between members from the persisted index (consumer → provider): `package_dep` (one repo imports a package the other publishes), `abi` (code references a Solidity contract defined in another repo), `http_route` (a fetch/axios call path matches a route registered elsewhere — anchored on persisted `HttpRoute` surface nodes unioned with the chunk scan), `graphql` (an executable GraphQL operation selects a root field another member's SDL schema defines; operation types must agree). Links attach at the feature (L1) level with evidence + provenance, and refresh **automatically** after `chaos_analyze`/`chaos_add` on any member (gated by the L2 repo root hash — a no-change re-index relinks nothing). `add_docs` indexes a directory of project-level documentation (ADRs, cross-repo design notes) as a docs-only member. `list` also returns **every indexed repository** — the discovery call when you don't know what Chaos already knows. | `action` (`create`\|`add_repo`\|`add_docs`\|`list`\|`status`\|`relink`), `project`, `repo`, `dir`, `alias`, `force` | To group client/backend/contracts/infra repos and see how features connect across them; then `chaos_features` with `project` for the cross-repo inventory. |
| `chaos_feature_story` | Tells the **cross-repo story of one feature** across a project: matches it in **every** member repo (L1 community search + lexical label fallback), traverses the persisted cross-repo links between the matched features (pulling in a link's other endpoint — e.g. the Solidity contract a client calls — even when the query didn't match it), orders the involved features into a journey-layer **spine** (entry → interface → core → foundation = client → backend → contracts), and writes a clickable **multi-page site** (an index page + one hash-gated drill-down page per involved feature) to the project workspace. Deterministic and embedder-light (one embed for the whole query, reused across repos). Returns a compact summary (involved repos, the ordered link chain, links-by-kind, not-involved repos, the site summary, provenance, `content_hash`). Distinct from `chaos_features --project` (an inventory of all features) and `chaos_compose` (one repo's composed page). | `project`, `feature`, `style`, `brand_preset`, `output_html`, `limit` | To answer "how does feature X work across **every** repo?" end-to-end. |

Agents should prefer MCP tools when available, and should not synthesize feature pages from
`chaos_query` alone when `chaos_feature_context` and `chaos_write_feature_website` are available. The
writer rejects README-like pages: feature pages must include interactive graph/story/code
navigation, architecture/flow sections, evidence, and a populated manifest. If
`chaos_feature_context` returns warnings about missing indexed subtrees or missing documentation
hits, refresh or re-target the context before writing a feature website.

## Supported languages

All non-Rust extraction uses **real AST parsers**, not regex or pattern matching. Each captures
functions, classes/structs, interfaces/traits, enums, type aliases, class methods, imports,
inheritance edges (Rust traits/impls, Solidity, and GraphQL `implements`), and (heuristic,
file-scoped) call edges.

| Language | Parser | Functions | Classes/Structs | Methods | Imports | Inheritance | Calls |
| --- | --- | :---: | :---: | :---: | :---: | :---: | :---: |
| Rust | `syn` | ✅ | ✅ | ✅ | ✅ | ✅ (traits/impls) | ✅ |
| TypeScript / JavaScript | `oxc` | ✅ | ✅ | ✅ | ✅ | ➖ | ✅ |
| Python | `rustpython-parser` | ✅ | ✅ | ✅ | ✅ | ➖ | ✅ |
| Solidity | `solang-parser` | ✅ | ✅ (contracts/libraries) | ✅ | ✅ | ✅ | ✅ |
| GraphQL | `apollo-parser` | ✅ (operations/fragments) | ✅ (type definitions) | ➖ | ➖ (none in the language) | ✅ (`implements`) | ✅ (fragment spreads) |

**GraphQL specifics.** Standalone `.graphql`/`.gql`/`.graphqls` files cover both halves of the
language: SDL type systems (object/input → struct, interface → trait, enum, union/scalar/directive
→ type alias, plus `extend` linked to its base by a `uses_type` edge tagged `relation: "extends"`)
and executable documents (`graphql_operation` / `graphql_fragment` nodes). Schema root fields
(`Query.user`, honoring custom `schema { query: … }` root mappings) become `graphql_field`
user-surface nodes — the anchors for `chaos_usage`, the `chaos_stack` API-surface section, and the
cross-repo `graphql` link kind. Documents embedded in host code are extracted too: `` gql`…` `` /
`` graphql`…` `` tagged templates and `gql(…)` calls in TS/JS, and `gql("…")` calls in Python — the
chunk body is the GraphQL document itself, anchored at the host file's lines. apollo-parser is
error-tolerant: a broken file still yields its recoverable definitions. Honest limits: SDL-derived
schemas only (code-first servers — async-graphql, TypeGraphQL, graphene/strawberry — are roadmap),
and an incremental `chaos add` resolves GraphQL cross-file edges only within the changed-file
batch (they return on the next full `chaos analyze`).

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

Core (L0) Postgres tables: `repositories`, `analysis_runs`, `files`, `nodes`, `edges`, `chunks`,
`embeddings`. The layered memory adds `communities`, `community_members`, `community_edges` (L1
god-nodes), `subtree_hash` columns (L2 Merkle rollup), `community_embeddings` (L3 summaries),
`community_summary_cache` (content-addressed summary reuse), and the cross-repo project tables
`projects`, `project_repos`, `cross_repo_links`. Migrations run via `sqlx::migrate!`
(`migrations/001…009` — `007_fk_indexes` adds covering FK indexes, `008_identifier_tokens`
rebuilds keyword search with identifier splitting, `009_project_docs` marks docs-only project
members) and are tracked in `_sqlx_migrations`. The `embeddings` table stores provider, model,
dimensions, content hash, and pgvector data.

**Per-repo isolation.** `repositories.root_path` is `UNIQUE`, and every file/node/edge/chunk/
community/embedding carries a `repo_id` foreign key (`on delete cascade`); all retrieval is scoped
`where repo_id = …`. So one Postgres database can hold many repositories without cross-talk, and
`chaos clean /absolute/path` cascades to exactly that repo's rows while `chaos clean` (no path)
wipes everything. The schema always survives — no `migrate` is needed before re-indexing.

## CLI

```bash
chaos help [<command>]                                 # agent-friendly guide: commands, workflows, examples (no DB/config needed)
chaos migrate                                          # create/update schema
chaos doctor                                           # check Postgres + embedding provider
chaos clean [<repo>] [--artifacts]                     # wipe the index (all repos, or one); --artifacts also deletes generated files
chaos analyze <repo>                                   # index a repository
chaos add <repo> [-m "<what changed>"]                 # index git-diff + refresh vault + write feature/bug page
chaos stats <repo>                                     # report index statistics (read-only, embedder-free)
chaos stack <repo>                                     # tech-stack + API-surface inventory (read-only, embedder-free)
chaos pages <repo>                                     # list the generated feature-memory pages — never ls
chaos gaps <repo> [--folder <prefix>] | --project <name>   # knowledge gaps: what retrieval can never find
chaos query <repo> "<question>" [--limit N] [--hierarchical]   # source-grounded answer (top-down with --hierarchical)
chaos feature-context <repo> "<task>" [--output-html page.html]
chaos impact <repo> "<feature>"                        # feature-vs-existing-code impact report + HTML
chaos usage <repo> "<symbol-or-surface-string>"        # who consumes X across subfolders (index-only, embedder-free)
chaos sui-migration-impact <repo> [--source auto]      # map an EVM/Solana repo's features onto Sui primitives
chaos change-plan <repo> "<change>" [--since <ref>]    # decompose a change into features + check order + HTML plan
chaos components <repo> ["<area>"]                     # explain a big area's core components (overview before features)
chaos features [<repo>] ["<filter>"] [--project <name>]   # list ALL features (folder | layer | topic auto-detect; --project spans repos)
chaos compose <repo> --sections features,correlations,stack [--persona "…"]   # ONE persona-tailored page (or a site) from KB sections
chaos project create|add-repo|add-docs|list|status|relink   # cross-repo projects (client + backend + contracts + infra)
chaos feature-story <project> "<feature>"              # cross-repo story of ONE feature as a multi-page site
chaos storyboard <repo> --manifest story.json          # render a client-facing user-story page (no code)
chaos graph <repo> [-o graph.html]                     # export interactive graph page
chaos obsidian <repo> [-o vault]                       # export Obsidian vault
chaos refresh <repo> [--all-features]                  # regenerate project-local artifacts
chaos setup [--dry-run] [--scope user|local|project]   # register MCP server in editors
chaos hook --event <PreToolUse|PostToolUse>
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
| [ARCHITECTURE.md](ARCHITECTURE.md) | System design: extraction, storage, retrieval, the layered-memory rationale, the feature-page manifest, and known limitations. |
| [RUNBOOK.md](RUNBOOK.md) | Canonical ops command reference for running, indexing, maintaining, and validating. |
| [CHANGELOG.md](CHANGELOG.md) | Release notes — what changed in each version and what to know when upgrading. |
| [docs/EDITOR_SETUP.md](docs/EDITOR_SETUP.md) | Canonical per-editor install (Claude Code / Codex / Windsurf / OpenCode), Claude Desktop, the plugin packages (marketplace / Cowork zip), and hooks. |
| [docs/OLLAMA_SETUP.md](docs/OLLAMA_SETUP.md) | Ollama install, model pull, and embedding troubleshooting. |

### Validation

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

For real repository indexing, configure either OpenAI or Ollama embeddings. If the embedder is
unavailable, analysis must fail rather than producing fake vectors.

### Key source files

`src/main.rs` (clap CLI), `src/mcp.rs` (MCP server, 24 tools), `src/pipeline.rs` (the shared
CLI/MCP pipelines — both surfaces call the same functions, so they cannot drift), `src/config.rs`
(toml+env config), `src/storage/` (Postgres via sqlx, one `Storage` type split by concern into
`repo`/`index`/`merkle`/`community`/`search`/`graph`/`project`/`stack_queries` modules; `search`
also defines the `ChunkSearch` port), `src/embedding.rs` (OpenAI/Ollama embedders behind the
`Embedder` port), `src/extractor.rs` (orchestration + Rust/Cargo/Markdown/PDF/JSON/AWS-CDK
extraction + call edges), `src/lang/{mod,javascript,python,solidity,graphql}.rs`
(oxc/rustpython/solang/apollo-parser AST extraction, including embedded `gql` documents),
`src/user_surface.rs` (env-var/HTTP-route/CLI-command/GraphQL-field surface nodes),
`src/weights.rs` (edge cost/confidence), `src/query.rs` (hybrid retrieval),
`src/community.rs` + `src/merkle.rs` + `src/community_summary.rs` + `src/layering.rs` +
`src/change_plan.rs` + `src/hierarchy_export.rs` (hierarchical memory: L1 Louvain god-nodes,
L2 Merkle rollup, L3 hash-gated community summaries, journey layering, change-plan tool, and
god-node/feature-map export), `src/feature_context.rs` + `src/feature_export.rs` (feature pages),
`src/stack.rs` (tech-stack + API-surface inventory), `src/pages.rs` (generated-page listing),
`src/gaps.rs` (knowledge-gap detection), `src/usage.rs` (consumer lookup),
`src/feature_inventory.rs` (the features inventory), `src/compose.rs` (composed pages/sites),
`src/feature_story.rs` (cross-repo feature stories), `src/sui_migration.rs` + `src/sui_docs.rs`
(Sui migration impact + compiled-in docs profile), `src/project.rs` + `src/linker.rs` (cross-repo
projects and the package_dep/abi/http_route/graphql linkers), `src/provenance.rs` (breadcrumbs),
`src/theme.rs` (shared report CSS/JS), `src/graph_export.rs` + `src/graph_serve.rs` (graph page +
live-search server), `src/obsidian_export.rs`, `src/setup.rs`, `src/hook.rs`,
`src/export_util.rs`, and `migrations/001_init.sql` (plus `002_communities.sql`,
`003_subtree_hash.sql`, `004_community_summary.sql`, `005_projects.sql`, `006_summary_cache.sql`,
`007_fk_indexes.sql`, `008_identifier_tokens.sql`, `009_project_docs.sql`).

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

Unless you state otherwise, any contribution you intentionally submit for inclusion in this work
shall be licensed under Apache-2.0, without any additional terms or conditions.
