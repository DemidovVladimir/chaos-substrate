# Plugin Installation Tutorial

Chaos Substrate ships as one repository that can be loaded as both a Codex plugin and a Claude Code
plugin. Do not copy `SKILL.md` into every target project.

## Package Layout

```text
chaos-substrate/
├── .codex-plugin/plugin.json
├── .claude-plugin/plugin.json
├── .mcp.json
├── bin/chaos-agent
├── skills/chaos-substrate/SKILL.md
└── scripts/chaos-agent
```

## One-Time Local Setup

From the Chaos Substrate checkout:

```bash
scripts/chaos-agent bootstrap
export PATH="$HOME/.local/bin:$PATH"
```

For local embeddings:

```bash
scripts/chaos-agent ollama-setup
CHAOS_CONFIG=chaos-substrate.local.toml scripts/chaos-agent bootstrap
export PATH="$HOME/.local/bin:$PATH"
```

## Per-Project Use

From any Rust, TypeScript, or JavaScript project:

```bash
chaos-agent onboard "$PWD"
chaos-agent context "$PWD" "authorization and RBAC"
chaos-agent explain "$PWD" "authorization and RBAC"
chaos-agent update "$PWD"
```

`onboard` writes portable `AGENTS.md` and `CLAUDE.md` sections, indexes the project, refreshes the
Obsidian vault, and keeps generated feature pages under `docs/features_memory`.

## Codex Plugin

Codex reads:

```text
.codex-plugin/plugin.json
```

The manifest points to the bundled skill directory and MCP config:

```text
skills/chaos-substrate/SKILL.md
.mcp.json
```

Use the plugin to let Codex choose Chaos Substrate for requests like:

```text
Go through this project and create sufficient index and explanation.
Generate an explanation website for the authorization feature.
Update the index before implementing this feature.
```

## Claude Code Plugin

Claude Code reads:

```text
.claude-plugin/plugin.json
```

For local development:

```bash
claude --plugin-dir /absolute/path/to/chaos-substrate
```

The plugin skill is namespaced:

```text
/chaos-substrate:chaos-substrate
```

To add the MCP server to a Claude Code project explicitly:

```bash
chaos-agent claude-code-add local "$PWD"
```

Use `project` instead of `local` when the `.mcp.json` should be shared with teammates.

## What Still Has To Exist

- Docker, if using the bundled Postgres/pgvector compose stack.
- Ollama or OpenAI embeddings.
- A pulled embedding model when using Ollama.

The plugin removes repeated manual wiring; it does not replace the database or embedding provider.
