# Plugin Installation Tutorial

Chaos Substrate ships as one repository that can be loaded as both a Codex plugin and a Claude Code
plugin. Do not copy `SKILL.md` into every target project.

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
├── .agents/plugins/marketplace.json
├── .claude-plugin/marketplace.json
├── .mcp.json
├── bin/chaos-agent
├── skills/chaos-substrate/SKILL.md
└── scripts/chaos-agent
```

## 2. Bootstrap Local Services

From the Chaos Substrate checkout:

```bash
CHAOS_CONFIG=chaos-substrate.local.toml scripts/chaos-agent bootstrap
export PATH="$HOME/.local/bin:$PATH"
```

`bootstrap` installs `chaos-agent`, starts Postgres, runs migrations, verifies the configured
embedder, and starts/pulls Ollama resources when the active config uses Ollama.

## 3. Install Or Enable In Codex

Add this repository as a Codex plugin marketplace:

```bash
codex plugin marketplace add /absolute/path/to/chaos-substrate
codex plugin marketplace list
```

Then restart Codex and install or enable `chaos-substrate` from the plugin UI if it is not enabled
automatically.

Codex reads:

```text
.agents/plugins/marketplace.json
.codex-plugin/plugin.json
skills/chaos-substrate/SKILL.md
.mcp.json
```

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

For a real marketplace install, add a marketplace that contains
`.claude-plugin/marketplace.json`, then install the plugin from Claude Code's `/plugin` UI. This
repository includes a local marketplace file at:

```text
.claude-plugin/marketplace.json
```

Claude Code reads:

```text
.claude-plugin/plugin.json
.claude-plugin/marketplace.json
skills/chaos-substrate/SKILL.md
.mcp.json
bin/chaos-agent
```

The shared MCP server exposes:

```text
chaos_analyze
chaos_query
chaos_feature_context
chaos_write_feature_website
```

## 6. Use From A Project

Only after the plugin is installed or loaded, open a Rust, Solidity, TypeScript, JavaScript, or Python project and ask
the agent:

```text
Use Chaos Substrate on this project and create an index plus explanation.
Update the Chaos Substrate index for this project.
Generate a feature explanation website for authorization and RBAC.
Find the implementation context I need before changing authorization and RBAC.
```

The plugin skill decides when to call `chaos-agent onboard`, `update`, `context`, or `explain`.
Those commands are implementation details, not the normal human interface.
When MCP is available, the plugin should prefer `chaos_analyze`, `chaos_query`, and
`chaos_feature_context` over shell commands. For feature websites, the agent should first use
`chaos_feature_context`, then compose the page and manifest, then call `chaos_write_feature_website`.
The writer rejects README-like pages; feature pages must include an interactive graph, story flow,
architecture and flow sections, code context, evidence/uncertainty, and a populated manifest.

Use the CLI directly only when you want to debug or run the workflow without an agent:

```bash
chaos-agent onboard "$PWD"
chaos-agent explain "$PWD" "authorization and RBAC"
```

Generated project artifacts stay under `chaos-obsidian-vault/` and `docs/features_memory/`.

## What Still Has To Exist

- Docker, if using the bundled Postgres/pgvector compose stack.
- Ollama or OpenAI embeddings.
- A reachable embedder; `bootstrap` pulls the local model when the active config uses Ollama.

The plugin removes repeated manual wiring; it does not replace the database or embedding provider.
