# Claude MCP Install: TypeScript Knowledge Base

This guide installs Chaos Substrate as a local MCP server for Claude and uses it to maintain a persistent knowledge base for a TypeScript or JavaScript repository.

## 1. Start Persistent Storage

From Chaos Substrate:

```sh
cd /absolute/path/to/chaos-substrate
docker compose up -d
```

The default local database is:

```text
postgres://chaos:chaos@localhost:54329/chaos_substrate
```

## 2. Configure Embeddings

OpenAI:

```sh
cp chaos-substrate.example.toml chaos-substrate.toml
export OPENAI_API_KEY="..."
```

Ollama:

```sh
cp chaos-substrate.local.toml chaos-substrate.toml
chaos-agent ollama-setup
```

Do not use fake embeddings. If neither OpenAI nor Ollama is available, indexing should fail.
For a fuller Ollama walkthrough, including install commands and dimension checks, see
`docs/OLLAMA_SETUP.md`.

## 3. Prepare The Database

```sh
cargo run -- migrate
cargo run -- doctor
```

`doctor` should report Postgres, pgvector, provider, model, and dimensions.

## 3.1 Build The Stable MCP Binary

Build once and point Claude at the binary directly. Do not use `cargo run` in Claude MCP config.

```sh
cargo build --release
./target/release/chaos --config chaos-substrate.toml doctor
```

## 4. Index A TypeScript Repository

```sh
cargo run -- analyze /absolute/path/to/typescript-repo
```

Re-run the same command whenever the repository changes:

```sh
cargo run -- analyze /absolute/path/to/typescript-repo
```

Current behavior replaces the stored index for that repository and reuses the same persisted database. The durable memory remains on disk in Postgres.

## 5. Test Query Locally

```sh
cargo run -- query /absolute/path/to/typescript-repo "where is request validation handled?"
```

Expected output includes relevant chunks with file paths, line ranges, scores, and graph context paths.

## 6. Export The Graph Webpage

Generate a standalone interactive graph page from the persisted index:

```sh
cargo run -- graph /absolute/path/to/typescript-repo --output graph.html
```

Open `graph.html` in a browser to validate the indexed graph visually. The page supports pan, zoom,
node-kind filters, search, draggable pinned nodes, and clickable node details with file path, source
line range, stable ID, chunk count, and metadata.

Use this page when checking whether indexing captured the expected files, symbols, dependencies,
imports, calls, and deployment resources. The export reads only Postgres graph data; it does not run
a web server or call the embedding provider.

For a full walkthrough, see `docs/GRAPH_WEBPAGE.md`.

## 7. Refresh Generated Views

Regenerate the project-local Obsidian vault from the persisted index:

```sh
cargo run -- refresh /absolute/path/to/typescript-repo
```

Focused feature websites are generated with `feature-context --output-html` and should be written to
`docs/features_memory`. These pages are for humans, but they also include a
`chaos-feature-manifest` JSON block for agents. This keeps generated feature memory separate from
normal docs.

Before implementing a related task, ask for focused context:

```sh
cargo run -- feature-context /absolute/path/to/typescript-repo "implement secure upload icon"
```

The command combines Postgres retrieval with matching feature manifests. It scans only direct HTML
files in `docs/features_memory`, not the whole documentation tree.

## 8. Add MCP To Claude Desktop

Add this server to Claude Desktop MCP config.

macOS config path is commonly:

```text
~/Library/Application Support/Claude/claude_desktop_config.json
```

Example:

```json
{
  "mcpServers": {
    "chaos-substrate": {
      "command": "/absolute/path/to/chaos-substrate/target/release/chaos",
      "args": [
        "--config",
        "/absolute/path/to/chaos-substrate/chaos-substrate.toml",
        "mcp"
      ],
      "env": {
        "DATABASE_URL": "postgres://chaos:chaos@localhost:54329/chaos_substrate",
        "OPENAI_API_KEY": "YOUR_KEY_IF_USING_OPENAI"
      }
    }
  }
}
```

For Ollama, omit `OPENAI_API_KEY` and ensure Ollama is running.

Template file:

```text
docs/claude_desktop_config.example.json
```

Restart Claude Desktop after editing the config.

## 9. Add MCP To Claude Code Or Cowork

For Claude Code, use the wrapper:

```sh
chaos-agent claude-code-add local /absolute/path/to/typescript-repo
```

Use `project` instead of `local` when you want a team-shared `.mcp.json` in the target repository:

```sh
chaos-agent claude-code-add project /absolute/path/to/typescript-repo
```

The path argument is the Claude Code project where `.mcp.json` should be written. If omitted, the
wrapper uses the current working directory.

For manual setup, copy `docs/claude_code_mcp.example.json` into the target repository as
`.mcp.json` and set `CHAOS_BIN`, `CHAOS_CONFIG`, and `DATABASE_URL` for each developer machine.

See `docs/CLAUDE_CODE_COWORK.md` for the full Claude Code / Cowork workflow.

## 10. Use From Claude

Ask Claude to use `chaos_analyze` to index and `chaos_query` to search.

Analyze/index:

```json
{
  "repo_path": "/absolute/path/to/typescript-repo"
}
```

Example:

```text
Use chaos_query on repo /absolute/path/to/typescript-repo.
Question: where is authentication middleware configured?
```

Tool input:

```json
{
  "repo": "/absolute/path/to/typescript-repo",
  "question": "where is authentication middleware configured?",
  "limit": 10
}
```

## 11. Claude Plugin, Skills, And Instructions

Claude Code does not consume Codex `.codex-plugin` metadata directly. Chaos Substrate also ships a
Claude plugin manifest:

```text
.claude-plugin/plugin.json
```

For local plugin testing:

```sh
claude --plugin-dir /absolute/path/to/chaos-substrate
```

The skill is then available as:

```text
/chaos-substrate:chaos-substrate
```

Use these surfaces together:

- `.claude-plugin/plugin.json` for reusable Claude plugin packaging.
- `.mcp.json` and `bin/chaos-agent` for plugin-level MCP.
- `CLAUDE.md` for target-project instructions written by `chaos-agent onboard`.
- `docs/CLAUDE_VALIDATION_BRIEF.md` for validation and PRD review.
- MCP `chaos_query` for live access to the persisted knowledge base.
- MCP `chaos_analyze` for indexing/reindexing repositories.
- MCP `chaos_feature_context` for feature explanations, implementation context, and optional HTML
  feature-page generation.

Codex-specific plugin files remain available:

- `.codex-plugin/plugin.json`
- `skills/chaos-substrate/SKILL.md`

## 12. Validate MCP Framing

Chaos Substrate MCP uses newline-delimited JSON-RPC over stdio. It must not emit `Content-Length` headers.

Smoke test:

```sh
printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}\n' \
  | ./target/release/chaos --config chaos-substrate.toml mcp
```

Expected result: one JSON response line.

## 13. Troubleshooting

If Claude cannot see the tool:

- Restart Claude Desktop.
- Confirm the config JSON is valid.
- Run `./target/release/chaos --config chaos-substrate.toml mcp` manually.
- Confirm `docker compose ps` shows Postgres running.
- Confirm `./target/release/chaos --config chaos-substrate.toml doctor` succeeds.

If indexing fails:

- Check OpenAI key or Ollama availability.
- Check embedding dimensions in `chaos-substrate.toml`.
- Do not bypass embedder failures with mock vectors.

If `graph.html` is empty or missing expected nodes:

- Confirm `cargo run -- query /absolute/path/to/typescript-repo "what files are indexed?"` returns results.
- Re-run `cargo run -- analyze /absolute/path/to/typescript-repo`.
- Check the `nodes` and `edges` table counts in Postgres.
- Make sure the `repo` argument matches the indexed absolute path or repository name.
