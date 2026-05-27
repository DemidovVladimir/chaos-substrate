# Plugin Workflow

The Chaos Substrate plugin should let an agent operate the memory system without asking users to
remember every command.

## What Still Requires User Setup

Users still need local infrastructure:

- Postgres with pgvector, normally via `docker compose up -d`
- a real embedder, either OpenAI credentials or a reachable Ollama embedding model

The plugin wrapper can start this repository's Docker Compose stack. It cannot install Docker,
download Ollama models, or create OpenAI credentials.

## Agent Commands

Use the wrapper from the Chaos Substrate plugin/repo root:

```bash
scripts/chaos-agent doctor
scripts/chaos-agent ollama-setup
scripts/chaos-agent init /absolute/path/to/project
scripts/chaos-agent update /absolute/path/to/project
scripts/chaos-agent context /absolute/path/to/project "authorization and RBAC"
scripts/chaos-agent explain /absolute/path/to/project "authorization and RBAC"
scripts/chaos-agent claude-code-add local
scripts/chaos-agent mcp
```

For local Ollama embeddings:

```bash
scripts/chaos-agent ollama-setup
CHAOS_CONFIG=chaos-substrate.local.toml scripts/chaos-agent init /absolute/path/to/project
```

See `docs/OLLAMA_SETUP.md` for installation and troubleshooting.

## Natural Language Mapping

- "Go through the project and create sufficient index and explanation"
  - Run `scripts/chaos-agent init <repo-path>`.
- "Update index"
  - Run `scripts/chaos-agent update <repo-path>`.
- "Generate explanation for X feature"
  - Run `scripts/chaos-agent explain <repo-path> "X"`.
- "Find context for implementing X"
  - Run `scripts/chaos-agent context <repo-path> "X"`.
- "Use this with Claude Code or Claude Cowork"
  - Run `scripts/chaos-agent claude-code-add local` for private setup or `project` for shared
    `.mcp.json`.

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

## Future Plugin Improvements

- Add a one-command installer that copies the plugin into the user's personal Codex plugin folder.
- Add an MCP server config template once plugin-relative binary paths are stable in the target app.
- Add query-generated feature maps, not only feature-context explanation pages.
