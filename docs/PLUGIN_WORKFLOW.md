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
- `bin/chaos-agent` delegates to `scripts/chaos-agent`, which owns setup, indexing, querying, and
  feature-page generation.

## What Still Requires User Setup

Users still need local infrastructure:

- Postgres with pgvector, normally via `docker compose up -d`
- a real embedder, either OpenAI credentials or Ollama installed locally

The plugin wrapper can start this repository's Docker Compose stack and can try to start Ollama plus
pull the configured model. It cannot install Docker, install the Ollama app, or create OpenAI
credentials.

## Agent Commands

From the Chaos Substrate plugin checkout, bootstrap once:

```bash
scripts/chaos-agent bootstrap
export PATH="$HOME/.local/bin:$PATH"
```

Then use the same command from any target project:

```bash
chaos-agent onboard /absolute/path/to/project
chaos-agent update /absolute/path/to/project
chaos-agent context /absolute/path/to/project "authorization and RBAC"
chaos-agent explain /absolute/path/to/project "authorization and RBAC"
chaos-agent claude-code-add local /absolute/path/to/project
```

For local Ollama embeddings:

```bash
CHAOS_CONFIG=chaos-substrate.local.toml scripts/chaos-agent bootstrap
export PATH="$HOME/.local/bin:$PATH"
CHAOS_CONFIG=chaos-substrate.local.toml chaos-agent onboard /absolute/path/to/project
```

See `docs/OLLAMA_SETUP.md` for installation and troubleshooting.

## Natural Language Mapping

- "Go through the project and create sufficient index and explanation"
  - Run `chaos-agent onboard <repo-path>` for first setup.
- "Update index"
  - Run `chaos-agent update <repo-path>`.
- "Generate explanation for X feature"
  - Run `chaos-agent explain <repo-path> "X"`.
- "Find context for implementing X"
  - Run `chaos-agent context <repo-path> "X"`.
- "Use this with Claude Code or Claude Cowork"
  - Run `chaos-agent claude-code-add local <repo-path>` for private setup or `project` for shared
    `.mcp.json`.

## Codex

Codex uses `.codex-plugin/plugin.json`. The manifest points to:

- `skills`: `./skills/`
- `mcpServers`: `./.mcp.json`

During development, install or load this repository as the `chaos-substrate` plugin. Once enabled,
the agent can use the `chaos-substrate` skill and the MCP tools exposed by `chaos-agent mcp`.

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
