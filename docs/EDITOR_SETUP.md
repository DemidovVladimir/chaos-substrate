# Editor Setup

Chaos Substrate registers as a stdio MCP server in every supported editor. This is the canonical
per-editor install reference. For the plugin package (zip/marketplace) flow, see
[docs/PLUGIN_INSTALL.md](PLUGIN_INSTALL.md).

All editor integrations launch the **release binary** directly over stdio:

```text
<abs>/target/release/chaos --config <abs>/chaos-substrate.toml mcp
```

Do not use `cargo run` in editor/MCP config. Build the release binary once (see Prerequisites), then
point each editor at it.

## One-Command Setup

The fastest path is the built-in `setup` subcommand. It auto-detects installed editors
(Claude Code / Codex / Cursor / Windsurf / OpenCode) and registers `chaos-substrate` as an MCP
server in each, merging into existing config rather than clobbering it.

```bash
# Preview every change without writing anything:
target/release/chaos setup --dry-run

# Apply to all detected editors:
target/release/chaos setup
```

The `--scope` flag **only affects the Claude Code `claude mcp add` registration**. The other
editors (Codex, Cursor, Windsurf, OpenCode) always write to their fixed user-level config files
regardless of `--scope`.

```bash
target/release/chaos setup --scope user      # Claude Code: user-level (default)
target/release/chaos setup --scope local     # Claude Code: machine-local
target/release/chaos setup --scope project   # Claude Code: project scope via claude mcp add
```

For a shareable project-scoped `.mcp.json` in a target repository, use the wrapper instead:

```bash
bin/chaos claude-code-add project /absolute/path/to/target-repo
```

`setup` is idempotent: rerunning it re-writes the chaos-substrate entry with the current values;
other MCP servers in the file are preserved. If an editor is not detected, it is skipped. The
manual blocks below are for editors `setup` cannot detect, or when you want to wire config by hand.

## Editor Support

| Editor      | MCP | Skills | Hooks | One-command `chaos setup` |
| ----------- | --- | ------ | ----- | ------------------------- |
| Claude Code | yes | yes    | yes   | yes                       |
| Codex       | yes | yes    | no    | yes                       |
| Cursor      | yes | no     | yes   | yes                       |
| Windsurf    | yes | no     | no    | yes                       |
| OpenCode    | yes | no     | no    | yes                       |

Skills ship via the plugin packages (`.claude-plugin` for Claude Code, `.codex-plugin` for Codex).
Hooks ship for Claude Code and Cursor (see [Hooks](#hooks)). All five editors get the same eighteen MCP
tools; see the [MCP Tools](../README.md#mcp-tools) section of the README for the tool reference.

## Prerequisites

1. **Postgres + pgvector.** Use the bundled stack (`docker compose up -d`) which starts
   `pgvector/pgvector:pg16` on host port `54329` with
   `DATABASE_URL=postgres://chaos:chaos@localhost:54329/chaos_substrate`.
2. **An embedder.** The example config (`chaos-substrate.example.toml`) defaults to local Ollama
   (`embeddinggemma`, 768 dims, `http://localhost:11434`) — no API key needed. Ollama must be
   running and the model pulled (`ollama pull embeddinggemma`). For OpenAI instead, uncomment the
   `open_ai` block in your config and set `OPENAI_API_KEY` (`text-embedding-3-small`, 1536 dims).
   Analysis fails closed if no real embedder is reachable.
3. **Build the release binary:**

   ```bash
   cargo build --release
   ```

4. **Migrate and verify the database:**

   ```bash
   target/release/chaos --config chaos-substrate.toml migrate
   target/release/chaos --config chaos-substrate.toml doctor
   ```

See the [Quick Start](../README.md#quick-start) section of the README for the full bootstrap
sequence.

## Manual Per-Editor Setup

In every block below, replace `<abs>` with the absolute path to your Chaos Substrate checkout and
`<cfg>` with the absolute path to your config file (for example
`<abs>/chaos-substrate.toml`).

### Claude Code

Use the wrapper, which builds the binary if needed and runs `claude mcp add` with the right scope:

```bash
bin/chaos claude-code-add local                       # private, machine-local
bin/chaos claude-code-add project /absolute/path/to/target-repo   # shareable .mcp.json
bin/chaos claude-code-add user                        # user-level config
```

Or register directly with the Claude Code CLI:

```bash
claude mcp add chaos-substrate -- <abs>/target/release/chaos --config <cfg> mcp
```

For the plugin (skills + hooks), add the local marketplace at `.claude-plugin/marketplace.json` and
install `chaos-substrate` from the `/plugin` UI. See [docs/PLUGIN_INSTALL.md](PLUGIN_INSTALL.md) for
the full marketplace and Cowork-zip flow.

### Codex

Register with the Codex CLI:

```bash
codex mcp add chaos-substrate -- <abs>/target/release/chaos --config <cfg> mcp
```

Or add the server block to `~/.codex/config.toml`:

```toml
[mcp_servers.chaos-substrate]
command = "<abs>/target/release/chaos"
args = ["--config", "<cfg>", "mcp"]
```

For skills, install the plugin via `.codex-plugin` and the
`.agents/plugins/marketplace.json` marketplace; see [docs/PLUGIN_INSTALL.md](PLUGIN_INSTALL.md).

### Cursor

Add the server to `~/.cursor/mcp.json` (merge into `mcpServers` if the file already exists):

```json
{
  "mcpServers": {
    "chaos-substrate": {
      "command": "<abs>/target/release/chaos",
      "args": ["--config", "<cfg>", "mcp"]
    }
  }
}
```

Cursor also reads project hooks from `.cursor/hooks.json` (see [Hooks](#hooks)).

### Windsurf

Windsurf is MCP-only (no skills or hooks). Add the server to
`~/.codeium/windsurf/mcp_config.json`:

```json
{
  "mcpServers": {
    "chaos-substrate": {
      "command": "<abs>/target/release/chaos",
      "args": ["--config", "<cfg>", "mcp"]
    }
  }
}
```

### OpenCode

OpenCode is MCP-only. Add a local MCP server to `~/.config/opencode/config.json`:

```json
{
  "mcp": {
    "chaos-substrate": {
      "type": "local",
      "command": ["<abs>/target/release/chaos", "--config", "<cfg>", "mcp"]
    }
  }
}
```

> Note: the Cursor, Windsurf, and OpenCode config paths above are the best-known locations as of
> 2026 and may need adjustment for your editor version. The Claude Code and Codex CLI commands are
> the most stable entry points.

## Hooks

The plugin ships hook configs that wire the `chaos hook` subcommand to inject code-memory context
into the agent on `Grep`, `Glob`, and `Bash` tool calls:

- Claude Code: `.claude-plugin/hooks/hooks.json` (`PreToolUse` on `Bash|Grep|Glob`, `PostToolUse`
  on `Bash`).
- Cursor: `.cursor/hooks.json` (same matchers, run with `--format cursor`).

`chaos hook` reads the editor's event JSON on stdin and emits memory context. It always exits 0 and
is a safe no-op when the database or index is unavailable, and it has no embedder dependency, so it
will not block tool calls or require OpenAI/Ollama to be running. The hooks launch the same release
binary as the MCP server.

## Verify

After registering an editor:

1. Confirm the database and embedder are healthy:

   ```bash
   target/release/chaos --config chaos-substrate.toml doctor
   ```

2. Index a repo and run a sample query (CLI mirror of the MCP tools):

   ```bash
   target/release/chaos --config chaos-substrate.toml analyze /path/to/repo
   target/release/chaos --config chaos-substrate.toml stats /path/to/repo
   target/release/chaos --config chaos-substrate.toml query /path/to/repo "where is the request handler validated?"
   ```

3. In the editor, confirm the eighteen MCP tools are listed: `chaos_analyze`, `chaos_add`,
   `chaos_stats`, `chaos_stack`, `chaos_query`, `chaos_feature_context`, `chaos_impact`,
   `chaos_write_feature_website`, `chaos_obsidian`, `chaos_refresh`, `chaos_write_storyboard`,
   `chaos_change_plan`, `chaos_components`, `chaos_features`, `chaos_project`, `chaos_help`, `chaos_clean`, and `chaos_graph`. See the
   [MCP Tools](../README.md#mcp-tools) section of the README for what each tool does.
