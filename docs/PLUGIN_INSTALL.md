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

## 4. Install Or Enable In Claude Code

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

## 5. Use From A Project

Only after the plugin is installed or loaded, open a Rust, TypeScript, or JavaScript project and ask
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
`chaos_feature_context` over shell commands. `chaos_feature_context` can also write the static HTML
feature page when `output_html` is provided and the MCP server has filesystem access.

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
