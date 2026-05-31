# Chaos Substrate Ops Runbook

Copy-paste operational reference for running Chaos Substrate — a portable, persistent
code-knowledge memory for AI agents (Postgres + pgvector), queried via the `chaos` CLI and a
stdio MCP server.

The binary is named `chaos`. The global flag `--config <PATH>` selects the config file
(default: `chaos-substrate.toml`).

- For MCP and plugin wiring, always launch the **release binary** directly over stdio
  (`target/release/chaos ... mcp`). Do **not** use `cargo run` in MCP/plugin config.
- `cargo run -- <subcommand>` is fine for one-off CLI work (bootstrap, ad-hoc queries).

Build the release binary once:

```sh
cargo build --release
# binary at: target/release/chaos
```

## Bootstrap

```sh
# 1. Start bundled Postgres + pgvector (pgvector/pgvector:pg16, host port 54329)
docker compose up -d

# 2. Provide a config (committed default targets Ollama)
cp chaos-substrate.example.toml chaos-substrate.toml   # if you keep an example; otherwise edit chaos-substrate.toml

# 3. Apply database migrations (sqlx::migrate!, tracked in _sqlx_migrations)
cargo run -- migrate
# or: target/release/chaos --config chaos-substrate.toml migrate

# 4. Verify connectivity, schema, and embedder
cargo run -- doctor
# or: target/release/chaos --config chaos-substrate.toml doctor
```

Default bundled connection:
`DATABASE_URL=postgres://chaos:chaos@localhost:54329/chaos_substrate`
(`DATABASE_URL` overrides the config file when set.)

## Index / Refresh

```sh
# Index (or re-index) a repository into the knowledge memory
chaos analyze /path/to/repo

# Refresh an already-indexed repo; optionally regenerate exports/features
chaos refresh /path/to/repo
chaos refresh /path/to/repo --obsidian-output vault
chaos refresh /path/to/repo --features-dir features
chaos refresh /path/to/repo --all-features
```

`analyze` requires a real embedder (OpenAI or Ollama). If none is configured, analysis
**fails by design** — never produces fake/random vectors.

## Clean / Reset

```sh
# Wipe every indexed repository from the database
chaos clean

# Wipe only one repository (by absolute path or repository name)
chaos clean /path/to/repo
```

`clean` removes persisted index data but leaves the schema in place — no `migrate` is needed
before re-indexing.

## Query

```sh
chaos query /path/to/repo "How does the embedder retry on failure?"
chaos query /path/to/repo "Where are call edges built?" --limit 20
```

`--limit N` controls the number of retrieved results (default 10).

## Feature Context

```sh
chaos feature-context /path/to/repo "Add a new language extractor"
chaos feature-context /path/to/repo "Add a new language extractor" --output-html out.html
chaos feature-context /path/to/repo "task" \
  --features-dir features \
  --output-html out.html \
  --limit 10 \
  --feature-limit 3 \
  --nodes-per-feature 8
```

Flags: `--limit N` (=10), `--feature-limit N` (=3), `--nodes-per-feature N` (=8),
`--features-dir P`, `--output-html P`.

## Exports

```sh
# Interactive HTML graph of nodes/edges
chaos graph /path/to/repo -o graph.html

# Obsidian vault export
chaos obsidian /path/to/repo
chaos obsidian /path/to/repo -o vault
```

## MCP Server

Run the MCP server over stdio (newline-delimited JSON-RPC, **no** Content-Length framing).
Use the release binary directly:

```sh
target/release/chaos --config chaos-substrate.toml mcp
```

Exposes exactly 4 tools: `chaos_analyze`, `chaos_query`, `chaos_feature_context`,
`chaos_write_feature_website` (see README.md "MCP Tools" for the full reference).

Validate the server responds with a single JSON line:

```sh
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"runbook","version":"0"}}}' \
  | target/release/chaos --config chaos-substrate.toml mcp
```

A correctly configured server prints one JSON-RPC response line to stdout.

## Editor Install

Auto-detect installed editors (Claude Code / Codex / Cursor / Windsurf / OpenCode) and register
chaos-substrate as an MCP server in each (merge-not-clobber):

```sh
chaos setup --dry-run                 # show what would change, write nothing
chaos setup                           # apply
chaos setup --scope user              # scope: user | local | project
chaos setup --scope project
```

Per-editor manual setup details: see `docs/EDITOR_SETUP.md`.

## Plugin Hook

`chaos hook` is the Claude Code / Cursor plugin hook. It reads event JSON on stdin and injects
code-memory context for Grep/Glob/Bash tool calls. It always exits 0 and is a safe no-op when the
DB/index is unavailable (no embedder dependency).

```sh
chaos hook --event PreToolUse
chaos hook --event PostToolUse
chaos hook --event PreToolUse --format claude     # format: claude | cursor
chaos hook --event PreToolUse --format cursor
```

Normally invoked by the editor, not by hand (the plugin ships `.claude-plugin/hooks/hooks.json`
and `.cursor/hooks.json`).

## Troubleshooting

- **Embedder not configured / analysis fails.** This is by design (fail-closed — no fake
  vectors). Configure a real embedder in `chaos-substrate.toml`:
  - OpenAI: `text-embedding-3-small` (1536 dims), needs `OPENAI_API_KEY`.
  - Ollama: `nomic-embed-text` (768 dims), `base_url http://localhost:11434`
    (committed default). Ensure the model is pulled: `ollama pull nomic-embed-text`.
  Re-run `chaos doctor` to confirm the embedder probe passes.

- **Postgres not reachable.** Confirm the container is up and the port is published:

  ```sh
  docker compose up -d
  docker compose ps
  ```

  Verify `DATABASE_URL` (or config) points at `postgres://chaos:chaos@localhost:54329/chaos_substrate`.
  `DATABASE_URL` overrides the config file when set.

- **Schema / migration issues.** Re-run migrations; they are tracked in `_sqlx_migrations`:

  ```sh
  chaos migrate
  ```

- **General health check.** `chaos doctor` probes database connectivity, schema/migrations,
  and the configured embedder. Run it first whenever something misbehaves.

- **Diagnostics vs. results.** Diagnostics (tracing) go to **stderr**; program results go to
  **stdout**. When capturing output, keep the streams separate.

## Validation (development)

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```
