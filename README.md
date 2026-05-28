# Chaos Substrate

Persistent code knowledge memory for agents.

The implementation is Rust-only code. It analyzes Rust, TypeScript, and JavaScript repositories, stores a source-grounded knowledge graph in Postgres, stores real embedding vectors in pgvector, and exposes hybrid query results through a CLI and stdio MCP server. It can also export a standalone `graph.html` page or an Obsidian vault for visual validation of the persisted graph.

## Guarantees

- Runtime code has no mock embedder and no random vectors.
- Indexed graph, chunks, and embeddings are persisted in Postgres/pgvector.
- Queries after restart use disk-backed memory.
- Chunks are symbol-aware retrieval projections, not the source of truth.
- Graph nodes and edges remain canonical for source grounding and context routing.

## Quick Start

```bash
cp chaos-substrate.example.toml chaos-substrate.toml
docker compose up -d
export OPENAI_API_KEY=...
cargo run -- migrate
cargo run -- doctor
cargo run -- analyze /path/to/repo
cargo run -- query /path/to/repo "where is the request handler validated?"
cargo run -- feature-context /path/to/repo "implement secure upload icon"
cargo run -- graph /path/to/repo --output graph.html
cargo run -- obsidian /path/to/repo --output chaos-obsidian-vault
cargo run -- refresh /path/to/repo
```

## Agent Plugin Workflow

Chaos Substrate is packaged as both a Codex plugin and a Claude Code plugin:

```text
.codex-plugin/plugin.json
.claude-plugin/plugin.json
skills/chaos-substrate/SKILL.md
.mcp.json
bin/chaos-agent
```

The plugin MCP server exposes `chaos_analyze`, `chaos_query`, `chaos_feature_context`, and
`chaos_write_feature_website`.

Install or load the plugin once per agent, then ask the agent to use Chaos Substrate from the target
project:

```bash
codex plugin marketplace add /absolute/path/to/chaos-substrate
claude --plugin-dir /absolute/path/to/chaos-substrate
scripts/package-cowork-plugin
```

Use the Codex command to add the local Codex marketplace. Use `claude --plugin-dir` to load the
plugin for a Claude Code session. For Claude Cowork, run `scripts/package-cowork-plugin` and upload
`dist/chaos-substrate-cowork-plugin.zip` from Claude Desktop -> Cowork -> Customize -> Plugins.
The Cowork zip includes the release MCP binary; rebuild and re-upload it after plugin changes.
After the plugin is installed or loaded, prompts like these become valid:

```text
Use Chaos Substrate on this project and create an index plus explanation.
Update the Chaos Substrate index.
Generate a feature explanation website for authorization and RBAC.
Find implementation context for authorization and RBAC.
```

The plugin skill maps those requests to the wrapper commands. Use the CLI directly only for
debugging or agentless operation:

```bash
scripts/chaos-agent bootstrap
export PATH="$HOME/.local/bin:$PATH"
chaos-agent onboard /absolute/path/to/project
chaos-agent explain /absolute/path/to/project "authorization and RBAC"
```

Natural language mapping for agents:

- "Set up Chaos here" -> plugin intent: onboard this project.
- "Go through the project and create sufficient index and explanation" -> plugin intent: create or refresh the project memory.
- "Update index" -> plugin intent: refresh the existing memory.
- "Generate explanation for X feature" -> plugin intent: create a focused feature-memory website.

The corresponding implementation commands are `chaos-agent onboard`, `update`, `context`, and
`explain`; users should not need to memorize them when the plugin is enabled.
When MCP is available, agents should prefer `chaos_analyze`, `chaos_query`, and
`chaos_feature_context`; feature websites should be LLM-composed from that evidence and written with
`chaos_write_feature_website`. The CLI wrapper is the fallback and setup path.
If `chaos_feature_context` returns warnings about missing indexed subtrees or missing documentation
hits, agents must refresh or target the context again before writing a feature website.
The writer rejects README-like pages; feature pages must include interactive graph/story/code
navigation, architecture/flow sections, evidence, and a populated manifest.

The wrapper builds the release binary if needed, starts the local Postgres container unless
`CHAOS_NO_DOCKER=1` is set, runs migrations, analyzes the repository, refreshes the Obsidian vault,
can write portable `AGENTS.md` / `CLAUDE.md` sections, and can write dark standalone
feature-context explanation websites.

Codex consumes `.codex-plugin/plugin.json`. Claude Code consumes `.claude-plugin/plugin.json`.
Both share the same `skills/`, `.mcp.json`, and `bin/chaos-agent` entrypoint.

For Claude MCP, build and launch the binary directly instead of using `cargo run`:

```bash
cargo build --release
./target/release/chaos --config chaos-substrate.toml mcp
```

For Ollama, use `chaos-substrate.local.toml` or edit `chaos-substrate.toml` to use `provider = "ollama"`.
`bootstrap` will run the Ollama readiness step before `doctor` or indexing.
The Ollama provider calls `/api/embed`, so use an Ollama version/model that supports embedding generation.

Fast Ollama path:

```bash
CHAOS_CONFIG=chaos-substrate.local.toml scripts/chaos-agent bootstrap
export PATH="$HOME/.local/bin:$PATH"
```

After bootstrap, use the plugin from the target project with natural requests such as "Use Chaos
Substrate on this project and create an index plus explanation."

See [docs/OLLAMA_SETUP.md](docs/OLLAMA_SETUP.md) for install, model pull, and troubleshooting steps.

## CLI

```bash
chaos migrate
chaos doctor
chaos analyze <repo-path>
chaos query <repo-or-path> "<question>"
chaos feature-context <repo-or-path> "<task>"
chaos graph <repo-or-path> --output graph.html
chaos obsidian <repo-or-path> --output chaos-obsidian-vault
chaos refresh <repo-or-path>
chaos mcp
```

`doctor` checks Postgres and performs a real embedding probe against the configured provider.

`analyze` extracts:

- Rust files and Cargo dependencies
- TypeScript/JavaScript files, package.json dependencies/scripts, tsconfig/jsconfig files, AWS CDK apps/stacks/resources
- Markdown/MDX docs as supplemental context with lower retrieval and graph weight than source code
- files, functions, classes, interfaces, type aliases, enums, structs, traits, impls, modules, tests
- source line ranges where available
- contains/imports/depends-on/calls graph edges
- symbol-aware chunks linked back to graph nodes
- real embeddings for every chunk

`graph` exports a standalone `graph.html` file for visual validation of the persisted nodes and
edges. Open it in a browser to pan, zoom, filter by node kind, search, and click nodes for source
metadata.

`obsidian` exports the same persisted graph as a local Obsidian vault. Open the output folder in
Obsidian to browse topic notes, node notes, backlinks, outgoing/incoming edges, and the generated
graph view.

`refresh` is the after-reindex command for project-local generated artifacts. It reads the current
persisted graph from Postgres and regenerates the repository Obsidian vault. Feature explanation
websites are generated from focused queries with `feature-context --output-html`.

`feature-context` builds an implementation brief for an agent. It combines live Postgres retrieval
with machine-readable manifests embedded in generated feature websites. Use it before implementing a
subfeature so related feature flows, code snippets, and page-backed relationships are included in the
agent context. Feature manifests are generic: they include feature metadata, claims, graph modes,
nodes, edges, story-step scopes, evidence, and confidence fields. Story steps should use explicit
`node_ids` and optional `edge_ids`; broad graph highlights belong in modes.

## Graph Webpage

Generate the page after indexing a repository:

```bash
cargo run -- graph /path/to/repo --output graph.html
```

The exporter reads persisted `nodes`, `edges`, `files`, and chunk counts from Postgres. It does not
run a web server, require Node.js, or call an embedding provider. The generated file is self-contained
and can be opened directly in a browser.

Use the webpage to validate:

- node coverage by type, file path, and source line range
- edge coverage for `contains`, `imports`, `depends_on`, `calls`, `defines`, and deployment links
- whether chunks are linked back to source graph nodes
- whether re-indexing changed the graph shape as expected

Interactive controls:

- search filters visible nodes by name, stable ID, kind, or file path
- kind checkboxes isolate files, symbols, dependencies, resources, and repository nodes
- mouse wheel zooms, dragging empty space pans, and dragging a node pins it
- clicking a node opens its metadata, source path, line range, chunk count, and stable ID

See [docs/GRAPH_WEBPAGE.md](docs/GRAPH_WEBPAGE.md) for the full setup and validation tutorial.

## Obsidian Vault

Generate a vault after indexing:

```bash
cargo run -- obsidian /path/to/repo --output chaos-obsidian-vault
```

The export writes `Topics/`, `Nodes/`, `Edges.md`, `README.md`, and `.obsidian/` settings. It does
not re-index the repository or call an embedding provider; it only reads the persisted graph from
Postgres.

See [docs/OBSIDIAN_EXPORT.md](docs/OBSIDIAN_EXPORT.md) for the Obsidian workflow.

## Refresh Generated Artifacts

After re-indexing a repository, refresh the generated project views:

```bash
cargo run -- refresh /absolute/path/to/repo
```

By default this writes:

- `/absolute/path/to/repo/chaos-obsidian-vault`

See [docs/REFRESH_EXPORTS.md](docs/REFRESH_EXPORTS.md) for the command reference.

## Feature Context

Before implementing a related task, ask Chaos Substrate for focused context:

```bash
cargo run -- feature-context /absolute/path/to/repo "implement secure upload icon"
```

The command returns:

- semantic and keyword hits from Postgres
- graph context paths around those hits
- relevant generated feature pages
- generic feature metadata, claims, graph modes, evidence, and confidence
- matched feature nodes, source snippets, and related edges from page manifests

Generated feature websites include a `<script type="application/json" id="chaos-feature-manifest">`
block specifically for agents. The visual DOM stays for humans; the manifest is the stable machine
contract. The command only scans direct `*.html` files in `docs/features_memory` by default and
ignores pages without this manifest, so it does not load the whole `docs/` tree.
Markdown/MDX docs indexed from the repository are shown separately as supplemental documentation
evidence when they match the task.

See [docs/FEATURE_CONTEXT.md](docs/FEATURE_CONTEXT.md) for the agent workflow.
See [docs/PLUGIN_WORKFLOW.md](docs/PLUGIN_WORKFLOW.md) for the plugin wrapper workflow.
See [docs/PLUGIN_INSTALL.md](docs/PLUGIN_INSTALL.md) for Codex and Claude plugin installation.
Open [docs/plugin-install.html](docs/plugin-install.html) for the dark visual tutorial.
See [docs/CLAUDE_CODE_COWORK.md](docs/CLAUDE_CODE_COWORK.md) for Claude Code and Cowork setup.

## Storage

Postgres tables:

- `repositories`
- `analysis_runs`
- `files`
- `nodes`
- `edges`
- `chunks`
- `embeddings`

The `embeddings` table stores provider, model, dimensions, content hash, and pgvector data. A dimension check prevents incompatible vectors from being stored.

## MCP

Run:

```bash
cargo run -- mcp
```

Tool:

```text
chaos_analyze(repo_path)
chaos_query(repo, question, limit)
chaos_feature_context(repo, task, limit, feature_limit, nodes_per_feature, features_dir)
chaos_write_feature_website(repo, slug, title, html, manifest)
```

See [docs/MCP_SETUP.md](docs/MCP_SETUP.md) and [docs/AGENT_VALIDATION.md](docs/AGENT_VALIDATION.md).
