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
| **Agent via MCP** | A stdio MCP server with 4 tools (`chaos_analyze`, `chaos_query`, `chaos_feature_context`, `chaos_write_feature_website`). | Coding agents (Claude Code, Codex, Cursor, Windsurf, OpenCode) that should query durable code memory instead of re-reading files. | `chaos setup` to register the server, then ask the agent to analyze and query. See [docs/EDITOR_SETUP.md](docs/EDITOR_SETUP.md). |
| **Raw CLI** | The `chaos` binary: `analyze`, `query`, `feature-context`, `graph`, `obsidian`, `refresh`. | Humans and scripts doing setup, debugging, one-off indexing, or agentless operation. | `chaos analyze <repo>` then `chaos query <repo> "<question>"`. See [Quick Start](#quick-start). |
| **Generated static feature-website** | A self-contained dark HTML feature page with interactive graph/story/code navigation plus a machine-readable manifest. | Sharing or reviewing how a feature works, and seeding future agent context from the embedded manifest. | `chaos feature-context <repo> "<task>" --output-html page.html`, or the `chaos_write_feature_website` MCP tool. |

## Quick Start

This is the canonical bootstrap. Bundled Postgres uses `pgvector/pgvector:pg16` on host port `54329`.

```bash
cp chaos-substrate.example.toml chaos-substrate.toml   # committed config defaults to Ollama
docker compose up -d                                   # pgvector on localhost:54329
export OPENAI_API_KEY=...                               # or run Ollama; see below
chaos migrate                                           # create schema (sqlx migrations)
chaos doctor                                            # check Postgres + real embedding probe
chaos analyze /path/to/repo                             # index the repository
chaos query /path/to/repo "where is the request handler validated?"
```

The default `DATABASE_URL` for the bundled container is
`postgres://chaos:chaos@localhost:54329/chaos_substrate`.

For Ollama, edit `chaos-substrate.toml` to use `provider = "ollama"` (base URL
`http://localhost:11434`, model `nomic-embed-text`, 768 dims). The committed config already defaults
to Ollama. See [docs/OLLAMA_SETUP.md](docs/OLLAMA_SETUP.md) for install, model pull, and
troubleshooting. OpenAI uses `text-embedding-3-small` (1536 dims, needs `OPENAI_API_KEY`).

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
exactly four tools. **This is the canonical tool reference.**

| Tool | What it does | Key params | When to use |
| --- | --- | --- | --- |
| `chaos_analyze` | Indexes or refreshes a repository into the persistent graph + embeddings. | `repo_path` | First, to build or update memory for a repo before querying. |
| `chaos_query` | Answers a focused, source-grounded question via hybrid (semantic + keyword) retrieval. | `repo`, `question`, `limit` (default 10) | To get a grounded answer about specific code without re-reading files. |
| `chaos_feature_context` | Gathers evidence for understanding a feature: semantic/keyword hits, graph context, feature-page manifests. | `repo`, `task`, `limit` (10), `feature_limit` (3), `nodes_per_feature` (8), `features_dir`, `output_html` | Before implementing or explaining a feature, to assemble an implementation brief. Pass `output_html` to also write the feature page. |
| `chaos_write_feature_website` | Writes an LLM-composed feature page plus its machine-readable manifest. | `repo`, `slug`, `title`, `html`, `manifest` | To persist a reviewed feature explanation as a shareable static page. |

Agents should prefer MCP tools when available, and should not synthesize feature pages from
`chaos_query` alone when `chaos_feature_context` and `chaos_write_feature_website` are available. The
writer rejects README-like pages: feature pages must include interactive graph/story/code
navigation, architecture/flow sections, evidence, and a populated manifest. If
`chaos_feature_context` returns warnings about missing indexed subtrees or missing documentation
hits, refresh or re-target the context before writing a feature website.

## Supported languages

All non-Rust extraction uses **real AST parsers**, not regex or pattern matching. Each captures
functions, classes/structs, interfaces/traits, enums, type aliases, class methods, imports,
inheritance, and (heuristic, file-scoped) call edges.

| Language | Parser | Functions | Classes/Structs | Methods | Imports | Inheritance | Calls |
| --- | --- | :---: | :---: | :---: | :---: | :---: | :---: |
| Rust | `syn` | ✅ | ✅ | ✅ | ✅ | ✅ (traits/impls) | ✅ |
| TypeScript / JavaScript | `oxc` | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Python | `rustpython-parser` | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
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
  (`nomic-embed-text`, 768 dims). Only chunk text is sent to the embedder.
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
chaos migrate                                          # create/update schema
chaos doctor                                           # check Postgres + embedding provider
chaos analyze <repo>                                   # index a repository
chaos query <repo> "<question>" [--limit N]            # source-grounded answer
chaos feature-context <repo> "<task>" [--output-html page.html]
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
| [llms.txt](llms.txt) | Machine-readable project summary for LLMs. |
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

`src/main.rs` (clap CLI), `src/mcp.rs` (MCP server, 4 tools), `src/config.rs` (toml+env config),
`src/storage.rs` (Postgres, sqlx), `src/embedding.rs` (OpenAI/Ollama embedders), `src/extractor.rs`
(orchestration + Rust/Cargo/Markdown/PDF/JSON/AWS-CDK extraction + call edges),
`src/lang/{mod,javascript,python,solidity}.rs` (oxc/rustpython/solang AST extraction),
`src/weights.rs` (edge cost/confidence), `src/query.rs` (hybrid retrieval),
`src/feature_context.rs` + `src/feature_export.rs` (feature pages), `src/graph_export.rs`,
`src/obsidian_export.rs`, `src/setup.rs`, `src/hook.rs`, `src/export_util.rs`, and
`migrations/001_init.sql`.
