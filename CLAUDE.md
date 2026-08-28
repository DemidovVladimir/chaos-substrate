# Chaos Substrate Agent Instructions

Chaos Substrate (1.0) is a small Rust crate that builds a persistent knowledge graph of one or
more code repositories so an agent can answer feature questions with code excerpts from the index
instead of re-reading source. Languages: Rust, TypeScript, JavaScript, Python, Solidity, GraphQL
(SDL, operations, and `gql`-embedded documents in TS/JS/Python), plus Markdown. Storage is
Postgres + pgvector; the memory survives process restarts.

## Hard rules

- No mock embedders, fake vectors, or random vectors. If the embedder is unavailable, indexing fails.
- Persistence stays Postgres + pgvector (`migrations/001_init.sql`). Never swap in in-memory storage.
- MCP stays on stdio with newline-delimited JSON-RPC 2.0.
- Runtime implementation is Rust only. All language extraction is tree-sitter in Rust — no Node or
  Python sidecars.
- On an indexed TARGET repository, answer feature questions with chaos tools only: no `rg`/`grep`/
  `ls` and no generated scripts against that repo. A missing capability is a chaos feature request.
- Hexagonal layering (enforced by the `layering` test in `src/main.rs`): `domain` depends on
  nothing in the crate; `ports` on `domain`; `application` on `domain` + `ports`; `adapters`
  implement ports; `entry` wires adapters into application. Never `use crate::adapters` from
  domain, ports, or application.

## The six tools

Every tool is a CLI subcommand (`chaos <name>`) and an MCP tool (`chaos_<name>`). Descriptions and
argument schemas live in one place: `src/application/tools.rs`.

- `index` — fully index a repository: tree-sitter extraction -> project-wide import/call
  resolution -> contextual chunks -> real embeddings -> feature communities -> Merkle hashes ->
  hash-gated summaries. Run once per repo.
- `reindex [--force]` — re-run a full index; a no-op when git HEAD equals the indexed commit and the
  worktree is clean. Unchanged chunks keep their embeddings (content-hash reuse).
- `add [--since <ref>]` — incrementally index what changed since the last index (content-hash
  drift, plus git's changes when `--since` is given); files gone from disk
  are purged; only new chunks are embedded. Use after every edit.
- `feature "<question>" [--budget N] [--no-persist] [--json]` — answer one feature question from
  the index without reading source: routed to matching features, code excerpts with exact line
  ranges, a 1-hop call/import graph, env vars and HTTP routes. Persists
  `docs/features_memory/<slug>.html` (hash-gated). Read `excerpts[].code` instead of opening files.
- `compose [--out <dir>] [--limit N]` — write the feature website (`docs/features_memory/site/`):
  an index plus one page per feature, content-hash gated. Cached pages must not be re-read.
- `correlations [--repos a b] [--min-confidence 0.5]` — intra-repo pairs of features sharing files
  or symbols, and cross-repo consumer->provider links (`package_dep`, `http_route`) across the
  repos of a project. Writes `correlations.html`.

CLI-only plumbing: `migrate [--reset]`, `doctor`, `mcp`, `help` (no config or DB needed).
Global flags: `--config <chaos-substrate.toml>`, `--project <chaos.toml>`, `--repo <alias|path>`, `-v`.

## Terms

- Chunk: one AST unit (function, struct, section) with a short context header, embedded as a vector.
- Community (feature, L1): a group of related nodes found by deterministic Louvain over the graph.
- Merkle hash (L2): content hashes rolled up chunk -> file -> community -> repo, so unchanged parts
  are recognised and skipped.
- Summary (L3): a templated text per community, embedded for routing; recomputed only when its
  community's hash changed.

## Configuration

- `chaos-substrate.toml` (`[storage] database_url`, `[embedding] provider|model|dimensions|base_url`,
  optional `[indexing] skip_dirs`). Env overrides: `DATABASE_URL`, `CHAOS_EMBED_PROVIDER`,
  `CHAOS_EMBED_MODEL`, `CHAOS_EMBED_DIMENSIONS`, `CHAOS_EMBED_BASE_URL`. Without a file,
  `DATABASE_URL` is required and the embedder defaults to Ollama `embeddinggemma` (768d).
- `chaos.toml` next to the repos defines a project: `name = "..."` plus `[[repos]]` entries with
  `alias` and `path`. It is a file, not a database entity. `--repo` accepts an alias from it.

## Workflow

1. `chaos doctor`, then `chaos migrate` (`--reset` to leave a legacy 0.x schema).
2. `chaos index` once per repo.
3. `chaos add` after every edit.
4. `chaos feature "<question>"` to answer from the index; `chaos compose` for the website;
   `chaos correlations` for cross-feature and cross-repo links.
5. `chaos reindex` only when the index looks stale.

## Validation

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

Legacy 0.24 code lives only at git tag `archive/v0.24-legacy`; do not reintroduce its tools.
