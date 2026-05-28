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
CHAOS_CONFIG=chaos-substrate.local.toml scripts/chaos-agent bootstrap
export PATH="$HOME/.local/bin:$PATH"
```

`bootstrap` calls `ollama-setup` automatically when the active config uses Ollama. It tries to start
the local server and pull `nomic-embed-text` when Ollama is installed.

## Project-Level Use

From any Rust, TypeScript, or JavaScript project, talk to the agent after the plugin is enabled:

```text
Use Chaos Substrate on this project and create an index plus explanation.
Update the Chaos Substrate index for this project.
Generate a feature explanation website for authorization and RBAC.
Find the implementation context I need before changing authorization and RBAC.
```

The plugin skill decides when to call `chaos-agent onboard`, `update`, `context`, or `explain`.
Those commands are implementation details, not the normal human interface.

Use the CLI directly only when you want to debug or run the workflow without an agent:

```bash
chaos-agent onboard "$PWD"
chaos-agent explain "$PWD" "authorization and RBAC"
```

Generated project artifacts stay under `chaos-obsidian-vault/` and `docs/features_memory/`.

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
- A reachable embedder; `bootstrap` pulls the local model when the active config uses Ollama.

The plugin removes repeated manual wiring; it does not replace the database or embedding provider.
