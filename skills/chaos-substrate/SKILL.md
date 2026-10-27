---
name: chaos-substrate
description: Use when installing, indexing, updating, or asking feature questions with Chaos Substrate in any Rust, Solidity, TypeScript, JavaScript, Python, or GraphQL repository (Markdown context included); Postgres+pgvector persistence, real Ollama/OpenAI embedders, CLI, MCP stdio, generated feature websites, and cross-repo correlations.
---

# Chaos Substrate

Chaos Substrate is a Rust-only code knowledge memory. It indexes repositories into a Postgres +
pgvector knowledge graph so that after one indexing run an agent answers feature questions from
the index — with code excerpts and exact line ranges — instead of re-reading source files.

Terms used below:

- **Chunk**: one AST unit (function, struct, method, Markdown section) plus a short context
  header, stored with a real embedding vector.
- **Community** (feature): a group of related graph nodes found by deterministic Louvain; each has
  a templated summary whose embedding routes questions to it.
- **Content hash**: the SHA-256 of a piece's own input. A chunk, a file, a feature summary and a
  generated page are each stored under it, so re-runs skip what did not change.

## When to use

- "Index this repo", "update the memory after my edits", "what does feature X do / how does Y
  work", "build the feature website", "how do the repos of this project connect".
- Any question about an INDEXED target repository: answer with chaos tools only. No `rg`, `grep`,
  `ls`, or generated scripts against that repo. A missing capability is a chaos feature request.

## Install and set up

1. Build: `cargo build --release` in the chaos-substrate checkout (binary `target/release/chaos`).
2. Postgres with pgvector reachable, and a real embedder: Ollama with `ollama pull embeddinggemma`
   (768 dims, the default) or OpenAI (`OPENAI_API_KEY`). No mock or random vectors exist.
3. Config: `chaos-substrate.toml` (copy `chaos-substrate.example.toml`) or environment variables:
   `DATABASE_URL`, `CHAOS_EMBED_PROVIDER` (`ollama` | `openai`), `CHAOS_EMBED_MODEL`,
   `CHAOS_EMBED_DIMENSIONS`, `CHAOS_EMBED_BASE_URL`. Without a file, `DATABASE_URL` is required.
4. `chaos doctor` (config, Postgres, embedder, git — exits non-zero on failure), then
   `chaos migrate` (`--reset` drops and recreates the schema; required to leave a legacy 0.x schema).
5. MCP: register `chaos mcp` as a stdio server (the repo ships `.mcp.json`). Tools appear as
   `chaos_index`, `chaos_reindex`, `chaos_add`, `chaos_feature`, `chaos_compose`, `chaos_correlations`.
6. Multi-repo project: put a `chaos.toml` next to the repos (a file, not a database entity):

```toml
name = "molecule"
[[repos]]
alias = "api"
path = "../api"
[[repos]]
alias = "web"
path = "../web"
```

Global CLI flags: `--config <chaos-substrate.toml>`, `--project <chaos.toml>` (default: nearest
ancestor of the cwd), `--repo <alias|path>` (default: cwd), `-v`/`-vv`. `chaos help` prints the
agent guide with no config or database.

## Workflow

1. `index` once per repository.
2. `add` after every edit (content-hash drift vs the index, or `--since <ref>` for a git range).
3. `feature "<question>"` to answer; read the returned excerpts, not the files.
4. `compose` to write the clickable feature website.
5. `correlations` to link features within a repo and across the project's repos.
6. `reindex` only when the index looks stale — it is a no-op when HEAD is indexed and clean.

Prefer MCP tools when available; the CLI subcommands take the same arguments.

## Tools

Argument tables are copied from `src/application/tools.rs` (the single source of truth).
`repo` everywhere: repository alias (from chaos.toml) or path; defaults to the current directory.

### index — `chaos index [--dry-run]` / `chaos_index`

Fully index a repository: tree-sitter structural extraction -> project-wide import/call resolution
-> contextual chunks -> real embeddings -> feature communities (L1) -> feature summaries (L2),
each embedded only when its text changed. Run once per repo; afterwards use `add`.

| arg | type | notes |
|---|---|---|
| `repo` | string | optional |

CLI-only `--dry-run`: extract, resolve and chunk; write nothing. Returns files, nodes, edges,
chunks, `chunks_embedded`, `chunks_reused`, communities, `summaries_recomputed`,
`call_resolution`, warnings, provenance, `skipped`.

### reindex — `chaos reindex [--force]` / `chaos_reindex`

Re-run a full index. No-op when git HEAD equals the indexed commit and the worktree is clean
(unless `force`). Unchanged chunks keep their embeddings (content-hash reuse).

| arg | type | notes |
|---|---|---|
| `repo` | string | optional |
| `force` | boolean | default `false` |

### add — `chaos add [--since <ref>]` / `chaos_add`

Incrementally index what changed since the last index: drifted files re-extracted against the persisted graph, deleted and
renamed-away files purged, only new chunks embedded, communities rebuilt and a summary embedded
again only where its text changed. Use after every edit instead of `reindex`.

| arg | type | notes |
|---|---|---|
| `repo` | string | optional |
| `since` | string | optional git ref: also re-index everything changed since it (e.g. `HEAD~1`); hash drift is always detected |

Returns `changed`, `purged`, nodes, chunks, `chunks_embedded`, `communities_changed`,
`summaries_recomputed`, warnings, provenance.

### feature — `chaos feature "<question>" [--budget N] [--no-persist] [--json] [--repos a b | --all-repos]` / `chaos_feature`

Answer ONE feature question from the index without reading source: routes to the matching feature
communities, retrieves the relevant chunks (literal identifiers named in the question over the
whole repo, then semantic + keyword over the routed files plus the literal hits' files), expands
one hop over call/import edges, and returns a token-bounded payload. Persists the answer as a page
with a visual feature map (`docs/features_memory/<slug>.html`, hash-gated).

With `repos` or `all_repos` the same question is answered in each selected repo of the project
(chaos.toml) and joined by cross-repo links — an excerpt's symbol name (identifier-shaped, ≥ 6
chars) found as a whole word in another repo's excerpt — on ONE page under
`<project dir>/docs/features_memory/`. Use it for features that span repos (contract → indexer →
UI → docs). Name the key identifiers in the question: they pull their files into scope.

| arg | type | notes |
|---|---|---|
| `question` | string | required, e.g. "how are embeddings reused on reindex" |
| `repo` | string | optional |
| `repos` | string[] | answer across these project repos (CLI `--repos`) |
| `all_repos` | boolean | answer across every project repo (CLI `--all-repos`) |
| `project` | string | chaos.toml for `repos`/`all_repos` (CLI global `--project`) |
| `budget_tokens` | integer | default 6000, min 1000, max 16000; across repos, split evenly (≥ 1000 each) (CLI `--budget`) |
| `persist` | boolean | default `true` (CLI `--no-persist`) |

Return fields: `features[]` (`id`, `label`, `score`, `summary`, `files`), `excerpts[]` (`path`,
`lines [start, end]`, `symbol`, `kind`, `score`, `why` = semantic/keyword/literal/graph, `code`,
`code_truncated`), `graph[]` (`from`, `kind`, `to`, `from_path`, `to_path`), `surfaces[]` (env
vars, HTTP routes), `page`, `page_cached`, `content_hash`, `tokens_used`, `warnings`,
`provenance`. Across repos: `question`, `project`, `repos[]` (`alias` + the fields above),
`links[]` (`from_repo`, `from`, `from_path`, `to_repo`, `to`, `to_path`, `term`), `page`
(absolute), `page_cached`, `content_hash`, `tokens_used`. The CLI prints Markdown by default;
`--json` prints the payload.

### compose — `chaos compose [--out <dir>] [--limit N]` / `chaos_compose`

Write the feature website of a repository: an index page listing every feature community plus one
drill-down page per feature (members, files, relations to other features, summary). Content-hash
gated per page.

| arg | type | notes |
|---|---|---|
| `repo` | string | optional |
| `out` | string | output directory; default `<repo>/docs/features_memory/site` |
| `limit` | integer | max feature pages, largest communities first |

Returns `out`, `pages_written`, `pages_cached`, `communities`, `omitted`, provenance.

### correlations — `chaos correlations [--repos a b] [--min-confidence 0.5]` / `chaos_correlations`

Feature correlations. Intra-repo: pairs of features sharing files or symbols. Cross-repo (project
from chaos.toml): consumer -> provider links by `package_dep` (one repo imports a package another
publishes) and `http_route` (a client call path matches a route another repo registers). Writes
`correlations.html` under `docs/features_memory` of the project directory (or of the single repo).

| arg | type | notes |
|---|---|---|
| `project` | string | path to chaos.toml; default nearest ancestor of the cwd (CLI `--project`) |
| `repos` | string[] | restrict to these aliases/paths (CLI `--repos`, or one `--repo`) |
| `min_confidence` | number | default 0.5; drops cross-repo links below it |

Returns `intra[]` (`repo`, `a`, `b`, `shared_files`, `shared_symbols`, `score`), `cross[]`
(`consumer_repo`, `consumer_feature`, `provider_repo`, `provider_feature`, `kind`, `evidence`,
`confidence`), `page` (`path`, `written`, `content_hash`), provenance. Every listed repo must be
indexed first.

## Token discipline

- Read `excerpts[].code` from the `feature` return instead of opening files. Each excerpt carries
  the exact `path` and `lines`; only open the source when the excerpt is `code_truncated` and the
  missing body matters. Raise `budget_tokens` (up to 16000) before falling back to file reads.
- `page_cached: true` on `feature`, and `pages_cached` on `compose`, mean the page already holds
  this content. Do not re-open or re-ingest cached pages; the return payload is the answer.
- Ask one focused question per `feature` call rather than one broad question with a large budget.
- Run `add` before asking about code you just changed; otherwise the answer describes the old code.
- If the embedder or Postgres is down, indexing fails loudly. Report the `doctor` failure; never
  substitute fake vectors or a script over the target repo.

## Development rules (working on Chaos itself)

- Hexagonal layers, enforced by the `layering` test in `src/main.rs`: `domain` (pure, no I/O) ->
  `ports` (traits) -> `application` (use cases) ; `adapters` implement ports; `entry` (cli, mcp,
  doctor) wires them. Never import `crate::adapters` from domain, ports, or application.
- Tool descriptions and schemas live only in `src/application/tools.rs`; CLI help, MCP
  `tools/list`, and the roster test all derive from it.
- Keep MCP on stdio, newline-delimited JSON-RPC 2.0. Keep persistence in Postgres + pgvector
  (`migrations/001_init.sql`). Keep extraction tree-sitter in Rust — no Node/Python sidecars.
- Validate: `cargo fmt --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`.
- Legacy 0.24 code exists only at git tag `archive/v0.24-legacy`; do not reintroduce it.
