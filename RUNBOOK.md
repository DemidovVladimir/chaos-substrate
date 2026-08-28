# Runbook

Operations reference for Chaos Substrate 1.0. See README.md for what it is and ARCHITECTURE.md
for how it works.

## Configuration

`chaos-substrate.toml` (path from `--config`, else `./chaos-substrate.toml`):

```toml
[storage]
database_url = "postgres://chaos:chaos@localhost:54329/chaos_substrate"

[embedding]
provider = "ollama"            # "ollama" | "open_ai"
model = "embeddinggemma"
dimensions = 768
base_url = "http://localhost:11434"   # optional; Ollama default shown

[indexing]                     # optional
skip_dirs = [".git", "target", "node_modules", ".venv", "dist", "build", ".next", "coverage"]
```

OpenAI instead: `provider = "open_ai"`, `model = "text-embedding-3-small"`, `dimensions = 1536`,
and `OPENAI_API_KEY` in the environment.

Environment variables override the file (a `.env` file is loaded too). Without any config file,
`DATABASE_URL` is required and the embedder defaults to Ollama `embeddinggemma` at 768 dims.

| Variable | Overrides |
| --- | --- |
| `DATABASE_URL` | `storage.database_url` |
| `CHAOS_EMBED_PROVIDER` | `embedding.provider` (`ollama` / `openai`) |
| `CHAOS_EMBED_MODEL` | `embedding.model` |
| `CHAOS_EMBED_DIMENSIONS` | `embedding.dimensions` |
| `CHAOS_EMBED_BASE_URL` | `embedding.base_url` |
| `OPENAI_API_KEY` | required by the OpenAI embedder |
| `CHAOS_SPLIT_COS` | cosine threshold for merging segments of oversize chunks (default 0.82) |
| `RUST_LOG` | log filter (else `-v` = debug, `-vv` = trace; logs go to stderr) |

`chaos.toml` (project): `name` plus `[[repos]]` entries with `alias` and `path` (absolute or
relative to the file). Found as the nearest ancestor of the current directory, or via
`--project <file-or-dir>`. `--repo <alias|path>` picks a repository; it defaults to the cwd.

## Ollama

```sh
ollama serve                    # if not already running (macOS/Windows: open the app)
ollama pull embeddinggemma      # 768 dims; keep dimensions = 768
curl http://localhost:11434/api/tags   # should list the model
```

Chaos calls `POST {base_url}/api/embed`. Any Ollama embedding model works as long as
`dimensions` matches what it returns (`nomic-embed-text` is also 768).

## Database

```sh
docker compose up -d            # pgvector/pgvector:pg16 on localhost:54329, volume chaos_substrate_pg
chaos migrate                   # applies migrations/001_init.sql (8 tables + the vector extension)
chaos migrate --reset           # drop schema public cascade, recreate, migrate — deletes every index
```

`migrate` refuses a database that holds the legacy 0.x schema (tables such as `projects`,
`cross_repo_links`, `analysis_runs`, `community_embeddings`, or `_sqlx_migrations` rows with
versions this binary does not ship) with a message pointing at `chaos migrate --reset`. The
reset wipes everything; a full `chaos index` of each repository follows. There is no in-place
upgrade from 0.x.

## Doctor

`chaos doctor` prints one `OK`/`FAIL` line per check and exits 2 if any fails:
`config` (parsed provider/model/dims), `postgres` (server version), `pgvector` (extension
installed), `schema` (migrated / not applied / legacy detected), `repos` (indexed repos with chunk
counts), `embedder` (a real embedding of "chaos doctor" and its dimension count), `git`.

## Indexing operations

| Command | Use when | Notes |
| --- | --- | --- |
| `chaos index [--repo R] [--dry-run]` | first time for a repo | `--dry-run` extracts, analyzes and chunks but writes nothing (the embedder is still used to split oversize units) |
| `chaos add [--repo R] [--since REF]` | after every edit | content-hash drift vs the index (plus git changes with `--since REF`); files gone from disk purged; only new chunks embedded; summaries hash-gated |
| `chaos reindex [--repo R] [--force]` | index looks stale | no-op when HEAD equals the indexed commit and the worktree is clean; embeddings reused by content hash |

All three return a JSON report: `files`, `nodes`, `edges`, `chunks`, `chunks_embedded`,
`chunks_reused`, `communities`, `summaries_recomputed`, `call_resolution` (resolved calls /
candidates), `warnings` (capped at 20), `provenance`, `skipped`, `dry_run`. Repositories are
identified by canonical root path; each repo is indexed independently, so a multi-repo project
is `chaos index --repo <alias>` per member.

Answering and pages:

```sh
chaos feature "how are embeddings reused on reindex" [--repo R] [--budget 6000] [--no-persist] [--json]
chaos compose [--repo R] [--out DIR] [--limit N]
chaos correlations [--project chaos.toml] [--repos api web] [--min-confidence 0.5]
```

## Page locations

| Page | Path |
| --- | --- |
| `feature` answer | `<repo>/docs/features_memory/<slug-of-question>.html` |
| `compose` site | `<repo>/docs/features_memory/site/index.html` + `features/<slug>.html` (or `--out`) |
| `correlations` report | `<project dir>/docs/features_memory/correlations.html`; without a project, inside the first repo |

Each page embeds its manifest as `<script type="application/json" id="chaos-manifest">` and is
content-hash gated: rewriting an unchanged page is a no-op, and `compose` reports `written` vs
`cached`. Pages are plain files; delete them freely, the next run recreates them.

## MCP server

`chaos --config <cfg> mcp` serves the six tools over stdio, newline-delimited JSON-RPC 2.0
(`initialize`, `ping`, `tools/list`, `tools/call`). Always launch the release binary with
absolute paths; never `cargo run`.

- Claude Code: `claude mcp add chaos-substrate -- <abs>/target/release/chaos --config <cfg> mcp`,
  or copy `docs/claude_code_mcp.example.json` into a project `.mcp.json`.
- Codex: `codex mcp add chaos-substrate -- <abs>/target/release/chaos --config <cfg> mcp`, or in
  `~/.codex/config.toml`: `[mcp_servers.chaos-substrate]` with `command` and `args`.
- Windsurf, `~/.codeium/windsurf/mcp_config.json`:
  `{"mcpServers": {"chaos-substrate": {"command": "<abs>/target/release/chaos", "args": ["--config", "<cfg>", "mcp"]}}}`
- OpenCode, `~/.config/opencode/config.json`:
  `{"mcp": {"chaos-substrate": {"type": "local", "command": ["<abs>/target/release/chaos", "--config", "<cfg>", "mcp"]}}}`

Verify with `chaos doctor`, then check the agent lists `chaos_index`, `chaos_reindex`,
`chaos_add`, `chaos_feature`, `chaos_compose`, `chaos_correlations`.

## Troubleshooting

**Embedder down** (`doctor` FAIL on `embedder`, or `index` fails with a connection error).
Indexing fails closed by design; there is no fake-vector fallback. Ollama: check
`curl http://localhost:11434/api/tags`, `ollama list` shows the model, `base_url` matches.
OpenAI: `OPENAI_API_KEY` set, network reachable. Requests time out after 300 s and retry 3 times.

**Dimension mismatch** (`embedder returned N dims, config says M`, or Postgres rejects an insert
on the `dimensions = vector_dims(embedding)` check). Set `dimensions` to exactly what the model
returns (`embeddinggemma` 768, `text-embedding-3-small` 1536). Switching models does not corrupt
the index: embeddings are keyed by (provider, model, dimensions), so the new model embeds every
chunk on the next `index`/`add` and summaries are recomputed because the gate key includes the
model id.

**No communities** (`communities: 0`, or `feature` warns it is falling back to whole-repo
search). Communities come from call/import edges; a repo with no resolvable edges (unsupported
language, only Markdown, or `--dry-run`) yields none. Check `edges` and `call_resolution` in the
index report, confirm the files are in a supported language and not under `skip_dirs`, then
`chaos reindex --force`. `feature` still answers from flat retrieval in the meantime.

**Repository not indexed yet.** `feature`, `compose` and `correlations` require a prior `index`
of that exact root path; run `chaos index --repo <path>` first.

**`reindex` did nothing.** It returned `skipped: true` because HEAD is the indexed commit and the
worktree is clean. Use `add` for uncommitted edits or `reindex --force`.

**Legacy schema detected.** `chaos migrate --reset`, then re-index every repository.
