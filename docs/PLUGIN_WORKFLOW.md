# Plugin Workflow

The Chaos Substrate plugin lets Codex and Claude Code operate the memory system without asking users
to copy skills into every project or remember every raw Cargo command.

## Plugin Package

The repository root is the plugin package:

```text
chaos-substrate/
├── .codex-plugin/plugin.json
├── .claude-plugin/plugin.json
├── .mcp.json
├── bin/chaos-agent
├── skills/chaos-substrate/SKILL.md
├── scripts/chaos-agent
├── docs/
└── src/
```

- Codex reads `.codex-plugin/plugin.json`.
- Claude Code reads `.claude-plugin/plugin.json`.
- Both share the root `.mcp.json`, `skills/`, and `bin/chaos-agent`.
- The shared MCP server exposes `chaos_analyze`, `chaos_query`, `chaos_feature_context`, and
  `chaos_write_feature_website`.
- `bin/chaos-agent` delegates to `scripts/chaos-agent`, which owns setup, indexing, querying, and
  feature-page generation.

## What Still Requires User Setup

Users still need local infrastructure:

- Postgres with pgvector, normally via `docker compose up -d`
- a real embedder, either OpenAI credentials or Ollama installed locally

The plugin wrapper can start this repository's Docker Compose stack and can try to start Ollama plus
pull the configured model. It cannot install Docker, install the Ollama app, or create OpenAI
credentials.

## Agent Workflow

From the Chaos Substrate plugin checkout, bootstrap once:

```bash
scripts/chaos-agent bootstrap
export PATH="$HOME/.local/bin:$PATH"
```

Then install or load the plugin in the agent:

```bash
codex plugin marketplace add /absolute/path/to/chaos-substrate
claude --plugin-dir /absolute/path/to/chaos-substrate
scripts/package-cowork-plugin
```

Use the Codex command for Codex. Use `claude --plugin-dir` for a Claude Code session. Use
`scripts/package-cowork-plugin` to create `dist/chaos-substrate-cowork-plugin.zip`, then upload that
zip in Claude Desktop -> Cowork -> Customize -> Plugins. Only after the plugin is installed or
loaded, from a target project, use natural agent requests:

If Cowork only sees `chaos_analyze` and `chaos_query`, the uploaded package is stale. Rebuild the
zip with `scripts/package-cowork-plugin` and upload it again; the MCP surface must include
`chaos_feature_context` and `chaos_write_feature_website`.

```text
Use Chaos Substrate on this project and create an index plus explanation.
Update the Chaos Substrate index.
Find implementation context for authorization and RBAC.
Generate a feature explanation website for authorization and RBAC.
```

The plugin skill chooses the underlying wrapper command. Direct CLI use is for debugging or
agentless operation:

```bash
chaos-agent onboard "$PWD"
chaos-agent explain "$PWD" "authorization and RBAC"
```

For local Ollama embeddings:

```bash
CHAOS_CONFIG=chaos-substrate.local.toml scripts/chaos-agent bootstrap
export PATH="$HOME/.local/bin:$PATH"
```

After that, use the same natural agent requests from the target project. The plugin will run
`chaos-agent onboard` or related commands with the active config.

See `docs/OLLAMA_SETUP.md` for installation and troubleshooting.

## Natural Language Mapping

- "Go through the project and create sufficient index and explanation"
  - Plugin intent: create or refresh the project memory. Implementation command: `chaos-agent onboard <repo-path>` for first setup.
- "Update index"
  - Plugin intent: refresh the existing memory. Implementation command: `chaos-agent update <repo-path>`.
- "Generate explanation for X feature"
  - Plugin intent: produce focused feature context and a feature-memory website. MCP flow: `chaos_feature_context`, then LLM-composed HTML/manifest via `chaos_write_feature_website`. CLI fallback: `chaos-agent explain <repo-path> "X"`.
- "Find context for implementing X"
  - Plugin intent: return source-grounded implementation context. MCP tool: `chaos_feature_context`. CLI fallback: `chaos-agent context <repo-path> "X"`.
- "Use this with Claude Code or Claude Cowork"
  - Run `chaos-agent claude-code-add local <repo-path>` for private setup or `project` for shared
    `.mcp.json`.

## Codex

Codex uses `.codex-plugin/plugin.json`. The manifest points to:

- `skills`: `./skills/`
- `mcpServers`: `./.mcp.json`

During development, install or load this repository as the `chaos-substrate` plugin. Once enabled,
the agent can use the `chaos-substrate` skill and the MCP tools exposed by `chaos-agent mcp`:
`chaos_analyze`, `chaos_query`, `chaos_feature_context`, and `chaos_write_feature_website`.

## Claude Code

Claude Code uses `.claude-plugin/plugin.json` and root-level plugin components. Test locally with:

```bash
claude --plugin-dir /absolute/path/to/chaos-substrate
```

Then in Claude Code:

```text
/chaos-substrate:chaos-substrate
```

The plugin also exposes `bin/chaos-agent` to Claude's shell environment while enabled.

## Outputs

Target project outputs:

- `chaos-obsidian-vault/`
- `docs/features_memory/*.html`
- `docs/features_memory/*-explanation.html`

Postgres remains the source of truth for indexed graph, chunks, and embeddings. Generated websites
are refreshable derived artifacts.

## Environment

- `CHAOS_CONFIG`: override config path.
- `CHAOS_BIN`: override binary path.
- `CHAOS_NO_DOCKER=1`: skip `docker compose up -d`.

## Plugin Limits

- The plugin can start this repository's Docker Compose stack, but it cannot install Docker itself.
- The plugin can try to start Ollama and pull the configured model when the Ollama CLI/app is
  installed, but it cannot install the Ollama desktop app.
- The plugin can configure Claude Code MCP with `chaos-agent claude-code-add`, but Claude Code may
  still ask for approval before using project-scoped MCP servers.
