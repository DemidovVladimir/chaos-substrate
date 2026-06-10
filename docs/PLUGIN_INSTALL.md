# Plugin Installation Tutorial

Chaos Substrate ships as one repository that can be loaded as both a Codex plugin and a Claude Code
plugin. Do not copy `SKILL.md` into every target project.

> For the fastest path, run `target/release/chaos setup` to auto-register the MCP server in every
> detected editor (Claude Code / Codex / Cursor / Windsurf / OpenCode), or
> `target/release/chaos setup --dry-run` to preview. This tutorial covers the **plugin package**
> (skills + hooks via marketplace/zip). For per-editor MCP-server registration details, see
> [docs/EDITOR_SETUP.md](EDITOR_SETUP.md).

## 1. Get The Plugin Package

Clone or update the Chaos Substrate repository:

```bash
git clone <chaos-substrate-repo-url>
cd chaos-substrate
```

The repository root is the plugin package:

```text
chaos-substrate/
├── .codex-plugin/plugin.json
├── .claude-plugin/plugin.json
├── .claude-plugin/hooks/hooks.json     # Claude Code tool-use hooks (chaos hook)
├── .cursor/hooks.json                  # Cursor tool-use hooks (chaos hook)
├── .agents/plugins/marketplace.json    # marketplace Codex reads
├── .claude-plugin/marketplace.json     # marketplace Claude Code reads
├── .mcp.json
├── bin/chaos
└── skills/chaos-substrate/SKILL.md
```

## 2. Bootstrap Local Services

From the Chaos Substrate checkout:

```bash
CHAOS_CONFIG=chaos-substrate.local.toml bin/chaos bootstrap
export PATH="$HOME/.local/bin:$PATH"
```

`bootstrap` installs `chaos`, starts Postgres, runs migrations, verifies the configured
embedder, and starts/pulls Ollama resources when the active config uses Ollama.

## 3. Install Or Enable In Codex

Add this repository as a Codex plugin marketplace:

```bash
codex plugin marketplace add /absolute/path/to/chaos-substrate
codex plugin marketplace list
```

Then restart Codex and install or enable `chaos-substrate` from the plugin UI if it is not enabled
automatically.

Codex reads the **`.agents/plugins/marketplace.json`** marketplace file, plus:

```text
.agents/plugins/marketplace.json
.codex-plugin/plugin.json
skills/chaos-substrate/SKILL.md
.mcp.json
```

To register only the MCP server (no plugin), see the Codex block in
[docs/EDITOR_SETUP.md](EDITOR_SETUP.md).

## 4. Install In Claude Cowork

Cowork does not use `claude --plugin-dir`. For Cowork, build a plugin zip and upload it through the
Claude Desktop UI.

From the Chaos Substrate checkout:

```bash
scripts/package-cowork-plugin
```

This writes:

```text
dist/chaos-substrate-cowork-plugin.zip
```

If the Claude CLI is installed, the package script runs `claude plugin validate` before writing the
zip.
The package script also builds and includes `target/release/chaos`, so the uploaded Cowork package
contains the MCP server binary that exposes `chaos_feature_context`.

In Claude Desktop:

1. Open the **Cowork** tab.
2. Open **Customize** in the sidebar.
3. Open **Plugins**.
4. Use the upload option on the Plugins page.
5. Select `dist/chaos-substrate-cowork-plugin.zip`.
6. After installation, type `/` or use the `+` button and verify the Chaos Substrate skill is
   available.

Cowork custom plugin upload is the path for a local file package. Claude's public docs state that
Cowork can install plugins from a file upload, and Claude Code docs state that zip archives are a
supported plugin package form for local plugin testing.

## 5. Install Or Enable In Claude Code

For local testing, start Claude Code with the plugin directory:

```bash
claude --plugin-dir /absolute/path/to/chaos-substrate
```

Inside Claude Code, verify the skill is visible:

```text
/chaos-substrate:chaos-substrate
```

For a real marketplace install, add a marketplace that contains the **`.claude-plugin/marketplace.json`**
file (this is the marketplace Claude Code reads, distinct from the `.agents/plugins/marketplace.json`
that Codex reads), then install the plugin from Claude Code's `/plugin` UI. This repository includes a
local marketplace file at:

```text
.claude-plugin/marketplace.json
```

Claude Code reads:

```text
.claude-plugin/plugin.json
.claude-plugin/marketplace.json
.claude-plugin/hooks/hooks.json
skills/chaos-substrate/SKILL.md
.mcp.json
bin/chaos
```

To register only the MCP server (no plugin), or for the `bin/chaos claude-code-add` and
`claude mcp add` commands, see [docs/EDITOR_SETUP.md](EDITOR_SETUP.md). The shared MCP server exposes
seventeen tools (`chaos_analyze`, `chaos_add`, `chaos_stats`, `chaos_query`, `chaos_feature_context`,
`chaos_impact`, `chaos_write_feature_website`, `chaos_obsidian`, `chaos_refresh`, `chaos_write_storyboard`, `chaos_change_plan`, `chaos_components`, `chaos_features`, `chaos_project`, `chaos_help`, `chaos_clean`, `chaos_graph`); see the [MCP Tools](../README.md#mcp-tools) section of the README for the
tool reference.

## 6. Use From A Project

Only after the plugin is installed or loaded, open a Rust, Solidity, TypeScript, JavaScript, or Python project and ask
the agent:

```text
Use Chaos Substrate on this project and create an index plus explanation.
Update the Chaos Substrate index for this project.
Generate a feature explanation website for authorization and RBAC.
Find the implementation context I need before changing authorization and RBAC.
```

The plugin skill decides when to call `chaos onboard`, `update`, `context`, or `explain`.
Those commands are implementation details, not the normal human interface.
When MCP is available, the plugin should prefer `chaos_analyze`, `chaos_query`, and
`chaos_feature_context` over shell commands. For feature websites, the agent should first use
`chaos_feature_context`, then compose the page and manifest, then call `chaos_write_feature_website`.
The writer rejects README-like pages; feature pages must include an interactive graph, story flow,
architecture and flow sections, code context, evidence/uncertainty, and a populated manifest.

Use the CLI directly only when you want to debug or run the workflow without an agent:

```bash
chaos onboard "$PWD"
chaos explain "$PWD" "authorization and RBAC"
```

Generated project artifacts stay under `chaos-obsidian-vault/` and `docs/features_memory/`.

## Hooks

The Claude Code plugin (`.claude-plugin/hooks/hooks.json`) and Cursor (`.cursor/hooks.json`) wire
the `chaos hook` subcommand to inject code-memory context on `Grep`, `Glob`, and `Bash` tool calls.
The hook always exits 0, is a safe no-op when the database or index is unavailable, and has no
embedder dependency. See [docs/EDITOR_SETUP.md](EDITOR_SETUP.md#hooks) for details.

## What Still Has To Exist

- Docker, if using the bundled Postgres/pgvector compose stack.
- Ollama or OpenAI embeddings.
- A reachable embedder; `bootstrap` pulls the local model when the active config uses Ollama.

The plugin removes repeated manual wiring; it does not replace the database or embedding provider.
