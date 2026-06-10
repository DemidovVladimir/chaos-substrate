# Plugin Workflow

The Chaos Substrate plugin lets Codex and Claude Code operate the memory system without asking users
to copy skills into every project or remember every raw Cargo command.

> One-command alternative: `target/release/chaos setup` auto-detects installed editors
> (Claude Code / Codex / Cursor / Windsurf / OpenCode) and registers the MCP server in each
> (merge-not-clobber; `--dry-run` to preview). For per-editor MCP registration and the tool-use
> hooks, see [docs/EDITOR_SETUP.md](EDITOR_SETUP.md).

## Plugin Package

The repository root is the plugin package:

```text
chaos-substrate/
├── .codex-plugin/plugin.json
├── .claude-plugin/plugin.json
├── .claude-plugin/hooks/hooks.json
├── .cursor/hooks.json
├── .mcp.json
├── bin/chaos
├── skills/chaos-substrate/SKILL.md
├── docs/
└── src/
```

- Codex reads `.codex-plugin/plugin.json`.
- Claude Code reads `.claude-plugin/plugin.json` and `.claude-plugin/hooks/hooks.json`.
- Both share the root `.mcp.json`, `skills/`, and `bin/chaos`.
- The shared MCP server exposes seventeen tools (`chaos_analyze`, `chaos_add`, `chaos_stats`,
  `chaos_query`, `chaos_feature_context`, `chaos_impact`, `chaos_write_feature_website`,
  `chaos_obsidian`, `chaos_refresh`, `chaos_write_storyboard`, `chaos_change_plan`, `chaos_components`, `chaos_features`, `chaos_project`, `chaos_help`, `chaos_clean`, `chaos_graph`); see the [MCP Tools](../README.md#mcp-tools) section of the
  README for the reference.
- The tool-use hooks (`.claude-plugin/hooks/hooks.json`, `.cursor/hooks.json`) run `chaos hook` to
  inject code-memory context on `Grep`, `Glob`, and `Bash`. The hook always exits 0, is a safe
  no-op when the DB/index is unavailable, and has no embedder dependency.
- `bin/chaos` is the single wrapper entrypoint: it owns setup, indexing, querying, and
  feature-page generation.

## What Still Requires User Setup

Users still need local infrastructure (Postgres + pgvector and a real embedder). See the
[Quick Start](../README.md#quick-start) section of the README for the bootstrap sequence and
[docs/EDITOR_SETUP.md](EDITOR_SETUP.md#prerequisites) for prerequisites.

The plugin wrapper can start this repository's Docker Compose stack and can try to start Ollama plus
pull the configured model. It cannot install Docker, install the Ollama app, or create OpenAI
credentials.

## Agent Workflow

From the Chaos Substrate plugin checkout, bootstrap once:

```bash
bin/chaos bootstrap
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
Add my latest changes to the Chaos Substrate memory and write a feature page.
Find implementation context for authorization and RBAC.
Generate a feature explanation website for authorization and RBAC.
```

For incremental work, `chaos add` (MCP `chaos_add`) is the one-shot path: it detects changed files
from git (working tree by default, `--since REF` for a committed range, `--path` for explicit files
including Markdown/Notion exports and PDFs), incrementally re-embeds only those changed chunks into
Postgres/pgvector, refreshes the Obsidian vault, and writes an interactive feature or bug HTML page
into `docs/features_memory` (feature vs bug auto-detected from branch + latest commit subject,
override with `--kind`). Cross-file call edges into unchanged files are not rebuilt incrementally;
run a full `chaos analyze` (or `chaos refresh`) for a complete graph rebuild.

The plugin skill chooses the underlying wrapper command. Direct CLI use is for debugging or
agentless operation:

```bash
chaos onboard "$PWD"
chaos explain "$PWD" "authorization and RBAC"
```

For local Ollama embeddings:

```bash
CHAOS_CONFIG=chaos-substrate.local.toml bin/chaos bootstrap
export PATH="$HOME/.local/bin:$PATH"
```

After that, use the same natural agent requests from the target project. The plugin will run
`chaos onboard` or related commands with the active config.

See `docs/OLLAMA_SETUP.md` for installation and troubleshooting.

## Natural Language Mapping

- "Go through the project and create sufficient index and explanation"
  - Plugin intent: create or refresh the project memory. Implementation command: `chaos onboard <repo-path>` for first setup.
- "Update index"
  - Plugin intent: refresh the existing memory. Implementation command: `chaos update <repo-path>`.
- "Add my latest changes" / "index what I just changed and write a page"
  - Plugin intent: one-shot incremental update from git diff. MCP tool: `chaos_add`. CLI: `chaos add [repo-path]`.
    Incrementally indexes only the changed files, refreshes the Obsidian vault, and writes a feature/bug page.
- "Show index stats" / "what did the analyze produce"
  - Plugin intent: report index statistics for an already-indexed repo (totals plus breakdowns of
    nodes, edges, chunks, and files), read-only and embedder-free. MCP tool: `chaos_stats`. CLI:
    `chaos stats <repo-path>`.
- "Generate explanation for X feature"
  - Plugin intent: produce focused feature context and a feature-memory website. MCP flow: `chaos_feature_context`, then LLM-composed HTML/manifest via `chaos_write_feature_website`. CLI fallback: `chaos explain <repo-path> "X"`.
- "Find context for implementing X"
  - Plugin intent: return source-grounded implementation context. MCP tool: `chaos_feature_context`. CLI fallback: `chaos context <repo-path> "X"`.
- "Show how X feature impacts the existing code"
  - Plugin intent: build a feature-vs-existing-code impact report for an indexed repo and always
    write an interactive HTML (impact summary + evidence) into `docs/features_memory`, returning a
    compact summary of the existing files/symbols the feature touches. MCP tool: `chaos_impact`.
    CLI: `chaos impact <repo-path> "X"`.
- "Use this with Claude Code or Claude Cowork"
  - Run `chaos claude-code-add local <repo-path>` for private setup or `project` for shared
    `.mcp.json`. See [docs/EDITOR_SETUP.md](EDITOR_SETUP.md) for all editors.
- "Set this up in my editor(s)"
  - Run `target/release/chaos setup` to register the MCP server in every detected editor, or
    `--dry-run` to preview. See [docs/EDITOR_SETUP.md](EDITOR_SETUP.md).

## Codex

Codex uses `.codex-plugin/plugin.json`. The manifest points to:

- `skills`: `./skills/`
- `mcpServers`: `./.mcp.json`

During development, install or load this repository as the `chaos-substrate` plugin. Once enabled,
the agent can use the `chaos-substrate` skill and the MCP tools exposed by `chaos mcp` (see
the [MCP Tools](../README.md#mcp-tools) section of the README for the seventeen-tool reference).

`chaos_write_feature_website` enforces the feature-page contract. It rejects prose-only pages that
do not include an interactive graph, story flow, architecture/flow sections, code context, evidence
panel, and a sufficiently populated manifest.

`chaos_feature_context` may return `warnings`. Agents must treat these as blockers for website
generation. Typical warnings mean a target subtree exists on disk but is missing from Postgres hits,
or that repository docs exist but no documentation evidence was retrieved. In that state the agent
should refresh the index or run a more targeted context query before calling
`chaos_write_feature_website`.

## Claude Code

Claude Code uses `.claude-plugin/plugin.json` and root-level plugin components. Test locally with:

```bash
claude --plugin-dir /absolute/path/to/chaos-substrate
```

Then in Claude Code:

```text
/chaos-substrate:chaos-substrate
```

The plugin also exposes `bin/chaos` to Claude's shell environment while enabled.

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
- The plugin can configure Claude Code MCP with `chaos claude-code-add`, but Claude Code may
  still ask for approval before using project-scoped MCP servers.
