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

`skip_dirs` matches directory basenames and replaces the default list. Foundry dependency
directories need no entry: every directory a `foundry.toml` names in `libs` (`lib/` when it names
none) is skipped by path, relative to that `foundry.toml`. `remappings.txt` and `foundry.toml`
are read for Solidity import resolution and never indexed.

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
chaos migrate                   # applies migrations/*.sql (8 tables + the vector extension; 003 adds the stamp columns)
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
| `chaos add [--repo R] [--since REF]` | after every edit, commit, pull or checkout | git delta since the stamp: stamped commit → HEAD, uncommitted now, uncommitted at the stamp (`--since REF` adds REF → HEAD); content-equal files skipped; removed files purged; only new chunks embedded; summaries hash-gated |
| `chaos reindex [--repo R] [--force]` | index looks stale | the `add` delta plus the files one hop upstream of each change; no-op when HEAD is the stamp and nothing is uncommitted; `--force` = full index; embeddings reused by content hash |

All three return a JSON report: `files`, `nodes`, `edges`, `chunks`, `chunks_embedded`,
`chunks_reused`, `communities`, `summaries_recomputed`, `call_resolution` (resolved calls /
candidates), `warnings` (capped at 20), `provenance`, `skipped`, `dry_run`. Repositories are
identified by canonical root path; each repo is indexed independently, so a multi-repo project
is `chaos index --repo <alias>` per member.

Answering and pages:

```sh
chaos feature "how are embeddings reused on reindex" [--repo R] [--budget 6000] [--no-persist] [--json] [--repos a b | --all-repos] [--entry Mutation.createLab]
chaos compose [--repo R] [--out DIR] [--limit N] [--repos a b | --all-repos]
chaos correlations [--project chaos.toml] [--repos api web] [--min-confidence 0.5]
```

## Page locations

| Page | Path |
| --- | --- |
| `feature` answer | `<repo>/docs/features_memory/<slug-of-question>.html` |
| `compose` site | `<repo>/docs/features_memory/site/index.html` + `features/<slug>.html` (or `--out`) |
| `feature --repos`/`--all-repos` answer | `<project dir>/docs/features_memory/<slug-of-question>.html` |
| `compose --repos`/`--all-repos` hub | `<project dir>/docs/features_memory/index.html`, plus each repo's site and the `correlations` report |
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

**`reindex` did nothing.** It returned `skipped: true` because HEAD is the stamped commit, the
worktree is clean, nothing was uncommitted at the last run and the indexing rules are unchanged.
After a Chaos upgrade that changes extraction, use `reindex --force`.

**`add` reconciled every file, or `reindex` ran in full.** The report's `delta.fallback` says why:
no stamp yet, a stamp written before 1.4.0 or left incomplete by a failed run (the run also
finishes embedding and the hierarchy), other indexing rules (`skip_dirs`, Foundry `libs` or a new
language), or the stamped commit is not in this clone. The next run uses the delta again.

**An edit `add` did not pick up.** `add` sees what git names: the diff from the stamped commit,
uncommitted and untracked files, and files uncommitted at the stamp. Edits to
`.git/info/exclude`, the global excludes, or a `.gitignore` above an indexed subdirectory are
invisible to it; so is a file edited and reverted while a run was in progress. Use
`reindex --force`.

**Entry points or other new metadata missing after an upgrade.** `add` re-extracts only files
whose content changed, so an extractor change does not reach unchanged files. Run
`chaos migrate`, then `chaos reindex --force` on each repo, then `compose`. Only chunks whose
text changed are embedded again. 1.4.0 changed extraction for Solidity, ABIs, GraphQL resolvers and long file heads, so
every repo needs it once. Rebuild every binary that reads the database (the MCP server too)
before that: a 1.3 binary fails on the node and edge kinds 1.4.0 writes.

**No `contract_abi` links, or a contract function with no callers.** The consumer's ABI must be
indexed: `*.abi`, `*.abi.json`, a `.json` under an `abi/` or `abis/` directory, or an ABI
constant in TS/JS. Git-ignored files (Foundry `out/` usually is) are never listed. A call whose
ABI the index does not hold (viem's `erc20Abi`) and calls in test files are never linked. The
`analyzed` log line on stderr counts `abi_uses` resolved/total; the File node's
`metadata.contract_calls` holds the calls found. Re-index both repos with 1.4.0.

**A Solidity function shows no selector** (`metadata.abi_unresolved` names the type). A
parameter type comes from code the index does not hold, such as a skipped Foundry lib. The
function is still indexed; only its ABI signature and selector are missing.

**A GraphQL field shows no resolver.** A resolver is found from a `switch (fieldName)` case in
TS/JS and a registration whose `typeName` and `fieldName` are literal strings (or same-file
`const`s). A field registered only when the CDK app runs, or through computed values, has no
resolver in the index; it is never guessed.

**`feature` does not start from my GraphQL field.** Write the coordinate (`Mutation.createLab`),
put a word such as query, mutation or resolver next to a bare root field name, or pass
`--entry`. The repo must hold the field's SDL or an operation that selects it.

**A page keeps its old layout after an upgrade.** The page hash covers the page's data, not the
renderer. Delete the generated page (or the whole `docs/features_memory/site/` folder) and run
`compose` again.

**Legacy schema detected.** `chaos migrate --reset`, then re-index every repository.
