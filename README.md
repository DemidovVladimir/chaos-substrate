# Chaos Substrate

Knowledge-graph memory for multi-repository projects. Index a repo once, then let an agent ask
feature questions and get back code excerpts from the index instead of re-reading source files.

Six tools. Rust only. Postgres + pgvector for storage. Real embeddings only (Ollama or OpenAI).

## Why

An agent working on a large project burns most of its tokens re-discovering the same code:
grepping, opening files, reading whole modules to answer "how does X work?". Chaos Substrate
does that discovery once, persists it, and answers later questions from the persisted index with
a token-bounded payload (feature summaries + exact code excerpts + a one-hop call/import graph).
After every edit, `add` re-indexes only the changed files, so the memory stays current cheaply.

## Quick start

```sh
docker compose up -d                 # Postgres 16 + pgvector on localhost:54329
ollama pull embeddinggemma           # local 768-dim embedder (Ollama must be running)
cargo build --release
cp chaos-substrate.example.toml chaos-substrate.toml
target/release/chaos migrate         # creates the 8-table schema
target/release/chaos doctor          # config, Postgres, pgvector, schema, embedder, git
target/release/chaos index --repo /absolute/path/to/repo
target/release/chaos feature --repo /absolute/path/to/repo "how are embeddings reused on reindex"
```

`chaos help` prints the agent guide (tools + workflow) and needs no database or config.

Supported languages: Rust, TypeScript, JavaScript, Python, Solidity, GraphQL (schemas,
operations, and `gql` documents embedded in TS/JS/Python), plus Markdown.

## The six tools

Every tool is a CLI subcommand (`chaos <name>`) and an MCP tool (`chaos_<name>`) with the same
arguments. `--repo` takes an alias from `chaos.toml` or a path; it defaults to the current
directory.

| Tool | What it does | Key arguments |
| --- | --- | --- |
| `index` | Full index: extraction → import/call resolution → contextual chunks → embeddings → feature communities → Merkle hashes → summaries. Run once per repo. | `repo`, `--dry-run` (CLI only) |
| `reindex` | Re-run a full index. No-op when git HEAD equals the indexed commit and the worktree is clean. Unchanged chunks keep their embeddings. | `repo`, `force` |
| `add` | Incremental index of what changed since the last index (content-hash drift; plus everything git reports with `--since REF`). Files gone from disk are purged; only new chunks are embedded. Use after every edit. | `repo`, `since` |
| `feature` | Answer one feature question from the index, without reading source. Returns feature summaries, code excerpts with line ranges, a 1-hop graph, env vars / HTTP routes involved. Persists the answer as a page. | `question`, `repo`, `budget_tokens` (1000–16000, default 6000), `persist` |
| `compose` | Write the feature website of a repo: an index page plus one page per feature community. Hash-gated per page. | `repo`, `out`, `limit` |
| `correlations` | Feature correlations. Intra-repo: features sharing files or symbols. Cross-repo (from `chaos.toml`): `package_dep` and `http_route` links, consumer → provider, with evidence and confidence. | `project`, `repos`, `min_confidence` (default 0.5) |

CLI-only plumbing: `migrate [--reset]`, `doctor`, `mcp`, `help`.

Terms used throughout: a **chunk** is one unit of code (usually one function/type) stored with a
context header and an embedding; a **community** (also called a *feature*) is a cluster of
symbols that call and import each other more than they touch the rest of the graph; a
**Merkle hash** is a hash of hashes, so a change deep inside a file changes the hash of every
level above it and nothing else.

## Multi-repo projects: `chaos.toml`

A project is a plain file next to the repositories, not a database entity. Chaos finds the
nearest `chaos.toml` above the current directory (or takes `--project <path>`).

```toml
name = "molecule"

[[repos]]
alias = "api"
path = "../api"        # absolute, or relative to this file

[[repos]]
alias = "web"
path = "../web"
```

Then `chaos index --repo api`, `chaos index --repo web`, `chaos correlations`.

## Register as an MCP server

Chaos speaks MCP over stdio (newline-delimited JSON-RPC). Point your agent at the release binary
directly; never `cargo run`. A project-scoped `.mcp.json` for Claude Code:

```json
{
  "mcpServers": {
    "chaos-substrate": {
      "type": "stdio",
      "command": "/absolute/path/to/chaos-substrate/target/release/chaos",
      "args": ["--config", "/absolute/path/to/chaos-substrate/chaos-substrate.toml", "mcp"],
      "env": { "DATABASE_URL": "postgres://chaos:chaos@localhost:54329/chaos_substrate" }
    }
  }
}
```

A copy with env-var placeholders lives in `docs/claude_code_mcp.example.json`. One-liners for
other agents (replace `<abs>` and `<cfg>` with absolute paths):

- Claude Code: `claude mcp add chaos-substrate -- <abs>/target/release/chaos --config <cfg> mcp`
- Codex: `codex mcp add chaos-substrate -- <abs>/target/release/chaos --config <cfg> mcp`
- Windsurf (`~/.codeium/windsurf/mcp_config.json`) and OpenCode (`~/.config/opencode/config.json`)
  take the same `command` + `args` shape; see RUNBOOK.md.

The server exposes exactly `chaos_index`, `chaos_reindex`, `chaos_add`, `chaos_feature`,
`chaos_compose`, `chaos_correlations`, and returns the agent guide in its `initialize` response.

## Contextual chunking

Every symbol tree-sitter extracts becomes one chunk: a short `//` context header (path, language,
line range, enclosing container, module, kind, name, what it uses and calls, the first line of its
doc comment) followed by the symbol body. The header is what makes a chunk retrievable on its own
and is part of its content hash. A unit larger than 6000 characters is split along its AST child
statements into segments of at least 400 characters; adjacent segments are embedded with the
configured encoder and merged while their cosine similarity is at least 0.82 and the merge stays
under 4500 characters, so parts follow meaning rather than a fixed line count. If the embedder
fails, the unit falls back to a plain line split. Each file also gets one `file_head` chunk for
imports and top-level statements no symbol owns.

## Generated pages

- `docs/features_memory/<slug>.html`: one page per `feature` answer.
- `docs/features_memory/site/`: the `compose` website (`index.html` + `features/<slug>.html`).
- `docs/features_memory/correlations.html`: the `correlations` report (next to `chaos.toml` for a
  project, inside the repo otherwise).

Every page embeds its manifest as JSON (`id="chaos-manifest"`) and is content-hash gated: an
unchanged index re-writes nothing.

## Validation

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

See ARCHITECTURE.md for the layering and the pipeline, RUNBOOK.md for operations and
troubleshooting. Licensed under Apache-2.0.
