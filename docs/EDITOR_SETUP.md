# Editor Setup

Chaos Substrate registers as a stdio MCP server in every supported editor. This is the canonical
per-editor install reference; the plugin-package (marketplace / Cowork-zip) flow lives in
[Plugin packages](#plugin-packages) below.

All editor integrations launch the **release binary** directly over stdio:

```text
<abs>/target/release/chaos --config <abs>/chaos-substrate.toml mcp
```

Do not use `cargo run` in editor/MCP config. Build the release binary once (see Prerequisites), then
point each editor at it.

## One-Command Setup

The fastest path is the built-in `setup` subcommand. It auto-detects installed editors
(Claude Code / Codex / Windsurf / OpenCode) and registers `chaos-substrate` as an MCP
server in each, merging into existing config rather than clobbering it.

```bash
# Preview every change without writing anything:
target/release/chaos setup --dry-run

# Apply to all detected editors:
target/release/chaos setup
```

The `--scope` flag **only affects the Claude Code `claude mcp add` registration**. The other
editors (Codex, Windsurf, OpenCode) always write to their fixed user-level config files
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
| Windsurf    | yes | no     | no    | yes                       |
| OpenCode    | yes | no     | no    | yes                       |

Skills ship via the plugin packages (`.claude-plugin` for Claude Code, `.codex-plugin` for Codex).
Hooks ship for Claude Code (see [Hooks](#hooks)). All four editors get the same twenty-four MCP
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

To wire a `.mcp.json` by hand, copy [`claude_code_mcp.example.json`](claude_code_mcp.example.json)
(stdio transport, env-var-defaulted binary/config/`DATABASE_URL`) into the project and fill in the
absolute paths. For the plugin (skills + hooks), see [Plugin packages](#plugin-packages) below.

### Claude Desktop

The desktop app reads `~/Library/Application Support/Claude/claude_desktop_config.json` on macOS
(`%APPDATA%\Claude\claude_desktop_config.json` on Windows). Add the same `mcpServers` block as the
Claude Code example — copy [`claude_code_mcp.example.json`](claude_code_mcp.example.json) into the
`mcpServers` object (it accepts the same shape), set the absolute binary/config paths and
`DATABASE_URL` (and `OPENAI_API_KEY` if you use OpenAI), then **restart Claude Desktop**.

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
`.agents/plugins/marketplace.json` marketplace; see [Plugin packages](#plugin-packages) below.

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

> Note: the Windsurf and OpenCode config paths above are the best-known locations as of
> 2026 and may need adjustment for your editor version. The Claude Code and Codex CLI commands are
> the most stable entry points.

## Hooks

The plugin ships hook configs that wire the `chaos hook` subcommand to inject code-memory context
into the agent on `Grep`, `Glob`, and `Bash` tool calls:

- Claude Code: `.claude-plugin/hooks/hooks.json` (`PreToolUse` on `Bash|Grep|Glob`, `PostToolUse`
  on `Bash`). Both events run the launcher `.claude-plugin/hooks/chaos-hook.sh`.

`chaos hook` reads the editor's event JSON on stdin and emits memory context. It always exits 0 and
is a safe no-op when the database or index is unavailable, and it has no embedder dependency, so it
will not block tool calls or require OpenAI/Ollama to be running.

The hooks do **not** hard-code a binary path. `chaos-hook.sh` self-locates from its own directory and
resolves the chaos binary in order — `$CHAOS_BIN`, then the checkout's `target/release/chaos`, then a
`chaos` on `PATH` — and **degrades to a silent no-op (exit 0, nothing on stderr) when no binary, config,
or database is found.** This is deliberate: a marketplace or zip install that has not built the binary,
or a machine where the database is down, must not spam `No such file or directory` on every tool call.
If you want the hooks active, put `chaos` on `PATH` (`bin/chaos install-agent`) or export `CHAOS_BIN`;
otherwise they stay dormant and harmless.

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

3. In the editor, confirm the twenty-four MCP tools are listed: `chaos_analyze`, `chaos_add`,
   `chaos_stats`, `chaos_stack`, `chaos_pages`, `chaos_gaps`, `chaos_query`, `chaos_feature_context`,
   `chaos_impact`, `chaos_usage`, `chaos_sui_migration_impact`, `chaos_write_feature_website`, `chaos_obsidian`,
   `chaos_refresh`, `chaos_write_storyboard`, `chaos_change_plan`, `chaos_components`, `chaos_features`,
   `chaos_compose`, `chaos_project`, `chaos_feature_story`, `chaos_help`, `chaos_clean`, and `chaos_graph`.
   See the [MCP Tools](../README.md#mcp-tools) section of the README for what each tool does.

## Plugin packages

Beyond the bare MCP server, Chaos ships as a plugin (skills + MCP tools + hooks). Each agent reads a
different manifest from the checkout:

- **Claude Code** reads `.claude-plugin/marketplace.json`, `.claude-plugin/plugin.json`, and
  `.claude-plugin/hooks/hooks.json`.
- **Codex** reads `.agents/plugins/marketplace.json` and `.codex-plugin/plugin.json`.

**Claude Code.** For local testing, launch with the plugin directory:

```bash
claude --plugin-dir /absolute/path/to/chaos-substrate
```

For a real install, add the local marketplace at `.claude-plugin/marketplace.json` and install
`chaos-substrate` from the `/plugin` UI.

**Codex.**

```bash
codex plugin marketplace add /absolute/path/to/chaos-substrate    # reads .agents/plugins/marketplace.json
codex plugin marketplace list
# then restart Codex and enable chaos-substrate from the plugin UI
```

**Claude Cowork (zip upload).** Build the self-contained package, then upload it in the desktop app:

```bash
scripts/package-cowork-plugin        # writes dist/chaos-substrate-cowork-plugin.zip, runs `claude plugin validate`
```

The zip bundles the freshly built `target/release/chaos`, so Cowork never depends on a stale binary.
Upload it via **Claude Desktop → Cowork → Customize → Plugins**. If Cowork only shows
`chaos_analyze` and `chaos_query`, the uploaded package is stale — rebuild with
`scripts/package-cowork-plugin` and re-upload.

Notes:

- Set `MAX_MCP_OUTPUT_TOKENS=50000` in the Claude environment if MCP responses are being truncated.
- A Cowork sandbox may not reach the host Postgres or write into the project tree. Prefer the host
  MCP tools; when a write (e.g. a feature page) is blocked, the tool returns its context and states
  that the write was blocked rather than pretending only the CLI works.
