# Chaos Substrate

Knowledge-graph memory for multi-repository projects. Index a repo once, then let an agent ask
feature questions and get back code excerpts from the index instead of re-reading source files.

Six tools. Rust only. Postgres + pgvector for storage. Real embeddings only (Ollama or OpenAI).

## Why

An agent working on a large project burns most of its tokens re-discovering the same code:
grepping, opening files, reading whole modules to answer "how does X work?". Chaos Substrate
does that discovery once, persists it, and answers later questions from the persisted index with
a token-bounded payload (feature summaries + exact code excerpts + a one-hop call/import graph).
After every edit, commit or checkout, `add` re-reads only the git delta since its last run (committed
and uncommitted), so the memory stays current cheaply.

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

Supported languages: Rust, TypeScript, JavaScript, Python, Solidity, contract ABIs, GraphQL
(schemas, operations, and `gql` documents embedded in TS/JS/Python), plus Markdown and
`.srt`/`.vtt` transcripts (cut into timestamped segments).

- Solidity: contracts, functions, modifiers, events, custom errors, state variables and
  file-level constants, with selectors computed from source and `emits` / `reverts` edges.
  Imports resolve through Foundry remappings (`remappings.txt`, `foundry.toml`); the Foundry
  dependency directories (`libs`, default `lib/`) are not indexed.
- Contract ABIs are found by path only: `*.abi`, `*.abi.json`, and `.json` files under a
  directory named `abi` or `abis`. ABI constants in TS/JS count too, and a viem/wagmi contract
  call is linked to the ABI it names (`uses_abi`).
- GraphQL: a `case "<field>":` of a `switch (fieldName)` in TS/JS (an AppSync resolver Lambda)
  is a `resolver` node that `resolves` its SDL field; a `{typeName, fieldName}` registration
  `binds` it.

Entry points, per content type: GraphQL root fields (with their resolvers), public contract
functions and getters (with selectors and the code that calls them), MCP tools, HTTP routes
(Next.js route handlers included), React data hooks, and for text the document plan and
transcript timestamps. `compose` lists them on every site index and on the project hub.

## The six tools

Every tool is a CLI subcommand (`chaos <name>`) and an MCP tool (`chaos_<name>`) with the same
arguments. `--repo` takes an alias from `chaos.toml` or a path; it defaults to the current
directory.

| Tool | What it does | Key arguments |
| --- | --- | --- |
| `index` | Full index: extraction → import/call resolution → contextual chunks → embeddings → feature communities → summaries. Run once per repo. | `repo`, `--dry-run` (CLI only) |
| `reindex` | Bring the index up to HEAD: the same git delta as `add` plus the files that import or reference a changed file. No-op when HEAD is the stamped commit and nothing is uncommitted. `force` runs a full index. Unchanged chunks keep their embeddings. | `repo`, `force` |
| `add` | Incremental index of the git delta since the last stamped run: the diff from the stamped commit to HEAD, files uncommitted now, and files uncommitted at the stamp (`--since REF` adds REF..HEAD). Content-equal files are skipped, removed ones purged, only new chunks embedded. Use after every edit, commit, pull or checkout. | `repo`, `since` |
| `feature` | Answer one feature question from the index, without reading source. Returns feature summaries, code excerpts with line ranges, a 1-hop graph, env vars / HTTP routes / MCP tools involved. Persists the answer as a page with a visual feature map. With `repos`/`all_repos`, answers across the project's repos on one page with cross-repo links. A question naming a GraphQL field (`Mutation.createLab`), or the `entry` argument, starts the answer at that field: SDL, registration, resolver, the calls behind it, and its callers. | `question`, `repo`, `repos`, `all_repos`, `project`, `budget_tokens` (1000–16000, default 6000; across repos, split evenly between them), `persist`, `entry` |
| `compose` | Write the feature website of a repo: an index page plus one page per feature community. Hash-gated per page. | `repo`, `out`, `limit` |
| `correlations` | Feature correlations. Intra-repo: features sharing files or symbols. Cross-repo (from `chaos.toml`): `package_dep`, `http_route`, `graphql_field` and `contract_abi` links, consumer → provider, with evidence and confidence. | `project`, `repos`, `min_confidence` (default 0.5) |

CLI-only plumbing: `migrate [--reset]`, `doctor`, `mcp`, `help`.

Terms used throughout: a **chunk** is one unit of code (usually one function/type) stored with a
context header and an embedding; a **community** (also called a *feature*) is a cluster of
symbols that call and import each other more than they touch the rest of the graph; a
**content hash** is the SHA-256 of a piece's own input. A chunk, a file, a feature summary and a
generated page are each stored under the hash of exactly what they are made from, so an
unchanged piece is recognised and its embedding (or page) is reused.

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
imports and top-level statements no symbol owns; a head over 1500 characters becomes `part i/n`
chunks (a test file keeps only the first).

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
